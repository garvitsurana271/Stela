//! DHCPv4/SLAAC admission, DAD and router-advertised DNS on one interface.
use super::*;
#[derive(Debug, Clone)]
pub struct Automatic {
    pub addresses: Vec<IpCidr>,
    pub dns: Vec<IpAddress>,
}

/// A unique local address (RFC 4193, fc00::/7): routed within a site only.
pub(super) fn unique_local(address: Ipv6Addr) -> bool {
    address.octets()[0] & 0xfe == 0xfc
}

/// A 6to4 (RFC 3056, 2002::/16) or Teredo (RFC 4380, 2001::/32) address:
/// global, but reached through a relay that is often gone, so RFC 6724
/// ranks it below IPv4. A Windows PC sharing its connection advertises a
/// 2002:: prefix with no DNS server.
pub(super) fn transition(address: Ipv6Addr) -> bool {
    let octets = address.octets();
    octets[..2] == [0x20, 0x02] || octets[..4] == [0x20, 0x01, 0, 0]
}

pub(super) fn link_local(mac: [u8; 6]) -> Ipv6Addr {
    let mut bytes = [0u8; 16];
    bytes[..2].copy_from_slice(&[0xfe, 0x80]);
    bytes[8..].copy_from_slice(&[
        mac[0] ^ 2,
        mac[1],
        mac[2],
        0xff,
        0xfe,
        mac[3],
        mac[4],
        mac[5],
    ]);
    Ipv6Addr::from(bytes)
}
impl<L: Link, C: Clock> Stack<L, C> {
    pub(super) fn update_dns(&mut self) -> Result<(), NetError> {
        let servers = match &self.fixed_dns {
            Some(servers) => servers.clone(),
            None => {
                let mut servers: Vec<IpAddress> = self
                    .config
                    .iter()
                    .flat_map(|config| config.dns.iter().copied().map(IpAddress::Ipv4))
                    .collect();
                let now = self.now()?;
                for server in self.phy.rdnss.servers(now) {
                    let address = IpAddress::Ipv6(server);
                    if !servers.contains(&address) {
                        servers.push(address);
                    }
                }
                servers
            }
        };
        if servers != self.dns_servers {
            self.sockets
                .get_mut::<dns::Socket>(self.dns)
                .update_servers(&servers);
            self.dns_servers = servers;
        }
        Ok(())
    }
    /// A default IPv6 route and an admitted address beyond the link: one
    /// that reaches at least the site.
    pub(super) fn ipv6_ready(&self) -> bool {
        self.ipv6_admitted(|_| true)
    }
    /// [`Self::ipv6_ready`] with a global address, not only a unique local
    /// one (RFC 4193, fc00::/7). A router may advertise a ULA prefix alone,
    /// with itself as the default route, on a network whose internet is
    /// IPv4: a name's global IPv6 address is unreachable from a ULA (RFC
    /// 6724 gives it a label of its own for that reason), so IPv4 is used.
    pub(super) fn ipv6_global_ready(&self) -> bool {
        self.ipv6_admitted(|address| !unique_local(address))
    }
    /// [`Self::ipv6_global_ready`] with a native global address, not only a
    /// 6to4 or Teredo one ([`transition`]), which IPv4 is preferred to.
    pub(super) fn ipv6_native_ready(&self) -> bool {
        self.ipv6_admitted(|address| !unique_local(address) && !transition(address))
    }
    /// Lines for the adapter's log once [`Self::automatic`] gave it an
    /// address: the DHCPv4 exchange when it has no IPv4 lease, so a log of
    /// an IPv6-only start says whether any offer came; and, when the
    /// network named no DNS server and no `--dns` is set, that lookups ask
    /// the public resolvers.
    pub fn admission_notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.config.is_none()
            && let Some(dhcp) = self.dhcp_summary()
        {
            notes.push(alloc::format!("no IPv4 lease; dhcp: {dhcp}"));
        }
        if self.fixed_dns.is_none() && self.dns_servers.is_empty() {
            let servers: Vec<String> = self
                .fallback_servers()
                .iter()
                .map(alloc::string::ToString::to_string)
                .collect();
            notes.push(match servers.is_empty() {
                true => String::from(
                    "no DNS server from DHCPv4 or IPv6 router advertisements, and no route to a public resolver",
                ),
                false => alloc::format!(
                    "no DNS server from DHCPv4 or IPv6 router advertisements: lookups ask the public resolvers {}",
                    servers.join(", ")
                ),
            });
        }
        notes
    }
    fn ipv6_admitted(&self, kind: impl Fn(Ipv6Addr) -> bool) -> bool {
        self.iface.routes().get_default_ipv6_route().is_some()
            && self.iface.ip_addrs().iter().any(|cidr| match cidr {
                IpCidr::Ipv6(cidr) => {
                    !cidr.address().is_unicast_link_local()
                        && kind(cidr.address())
                        && self.phy.approved_ipv6.contains(&cidr.address())
                }
                _ => false,
            })
    }
    pub(super) fn address_usable(&self, address: IpAddress) -> bool {
        match address {
            IpAddress::Ipv4(_) => self.config.is_some(),
            IpAddress::Ipv6(address) => {
                if address.is_unicast_link_local() {
                    self.phy.approved_ipv6.contains(&self.phy.local_ipv6)
                } else if unique_local(address) {
                    self.ipv6_ready()
                } else {
                    self.ipv6_global_ready()
                }
            }
        }
    }
    pub(super) fn probe_ipv6(&mut self) -> Result<(), NetError> {
        // Prefix withdrawal/expiry must also retire admission. Otherwise a
        // later reuse of the address could bypass its new DAD interval.
        self.phy.approved_ipv6.retain(|address| {
            self.iface
                .ip_addrs()
                .iter()
                .any(|cidr| matches!(cidr, IpCidr::Ipv6(cidr) if cidr.address() == *address))
        });
        let now = self.now()?;
        let mut completed = Vec::new();
        self.phy.probes.retain(|probe| {
            if !self
                .iface
                .ip_addrs()
                .iter()
                .any(|cidr| matches!(cidr, IpCidr::Ipv6(cidr) if cidr.address() == probe.address))
            {
                return false;
            }
            if probe.duplicate || now >= probe.until {
                completed.push((probe.address, probe.duplicate));
                false
            } else {
                true
            }
        });
        for (address, duplicate) in completed {
            if duplicate {
                if self.conflicts.len() >= 64 {
                    return Err(NetError::Invalid("IPv6 conflict inventory capacity"));
                }
                self.conflicts.push(address);
            } else {
                self.phy.approved_ipv6.push(address);
            }
        }
        let candidates: Vec<_> = self
            .iface
            .ip_addrs()
            .iter()
            .filter_map(|cidr| match cidr {
                IpCidr::Ipv6(cidr)
                    if !self.phy.approved_ipv6.contains(&cidr.address())
                        && !self.conflicts.contains(&cidr.address())
                        && !self
                            .phy
                            .probes
                            .iter()
                            .any(|probe| probe.address == cidr.address()) =>
                {
                    Some(cidr.address())
                }
                _ => None,
            })
            .collect();
        for address in candidates {
            if address != self.phy.local_ipv6
                && !self.phy.approved_ipv6.contains(&self.phy.local_ipv6)
            {
                continue;
            }
            self.probe_nonce = self.probe_nonce.wrapping_add(1);
            let mut nonce = [0; 6];
            nonce.copy_from_slice(&self.probe_nonce.to_le_bytes()[..6]);
            self.phy
                .link
                .transmit(&dad::solicitation(self.phy.link.mac().0, address, nonce))?;
            // DAD is asynchronous: every normal poll still services DHCP,
            // DNS/TCP, router lifetimes and unrelated received frames.
            let until = self.now()?.saturating_add(1_000_000);
            self.phy.probes.push(dad::Probe {
                address,
                nonce,
                // Link admission/queue contention may have consumed time before
                // publication. Keep the full DAD interval after transmit returns.
                until,
                duplicate: false,
            });
        }
        // Updating even an unchanged address list flushes smoltcp's ARP/ND
        // cache. Change it only for a rejected global candidate; a
        // normal poll must not disrupt unrelated established TCP connections.
        let remove = self.iface.ip_addrs().iter().any(|cidr| {
            matches!(cidr, IpCidr::Ipv6(cidr) if cidr.address() != self.phy.local_ipv6 && self.conflicts.contains(&cidr.address()))
        });
        if remove {
            self.iface.update_ip_addrs(|addresses| {
                addresses.retain(|cidr| match cidr {
                    IpCidr::Ipv6(cidr) => {
                        cidr.address() == self.phy.local_ipv6
                            || !self.conflicts.contains(&cidr.address())
                    }
                    _ => true,
                })
            });
        }
        Ok(())
    }
    /// Configure DHCPv4 and IPv6 SLAAC together. IPv6 is admitted after DAD
    /// and a router advertisement; DNS also accepts RFC8106 RDNSS lifetimes.
    /// Stateful DHCPv6 is not implemented.
    pub fn automatic(&mut self) -> Result<Automatic, NetError> {
        self.automatic_waiting(|_| {})
    }

    /// [`Self::automatic`], telling `waiting` each whole second it has
    /// waited for the link's carrier so far.
    pub fn automatic_waiting(
        &mut self,
        mut waiting: impl FnMut(u64),
    ) -> Result<Automatic, NetError> {
        self.await_carrier(&mut waiting)?;
        let deadline = self.now()?.saturating_add(DHCP_WAIT_US);
        loop {
            self.poll()?;
            // IPv4, or IPv6 with a native global address and a DNS server,
            // is ready at once. IPv6 with only a unique local, 6to4 or
            // Teredo address, or with no DNS server, waits for a DHCPv4
            // lease until the deadline, and is taken then: before, an
            // install asked the IPSW's global address from a ULA and every
            // range timed out while the lease came seconds later, and one
            // took a 2002:: address with no DNS server 37 s in and never
            // waited for DHCPv4.
            if self.config.is_some()
                || (self.ipv6_native_ready() && !self.dns_servers.is_empty())
                || (self.ipv6_ready() && self.now()? >= deadline)
            {
                return Ok(Automatic {
                    addresses: self
                        .iface
                        .ip_addrs()
                        .iter()
                        .filter(|cidr| match cidr {
                            IpCidr::Ipv4(_) => true,
                            IpCidr::Ipv6(cidr) => self.phy.approved_ipv6.contains(&cidr.address()),
                        })
                        .copied()
                        .collect(),
                    dns: self.dns_servers.clone(),
                });
            }
            if self.now()? >= deadline {
                if let Some(address) = self.conflicts.first() {
                    return Err(NetError::DuplicateAddress(*address));
                }
                return Err(NetError::Dhcp(
                    "no DHCPv4 lease or usable IPv6 router advertisement within 60 seconds; stateful DHCPv6 is unsupported",
                ));
            }
            self.wait_until(deadline)?;
        }
    }
}
pub(super) fn authority(host: &str) -> Result<(&str, u16), NetError> {
    if let Some(rest) = host.strip_prefix('[') {
        let (address, suffix) = rest
            .split_once(']')
            .ok_or(NetError::Invalid("IPv6 authority"))?;
        address
            .parse::<Ipv6Addr>()
            .map_err(|_| NetError::Invalid("IPv6 authority"))?;
        let port = if suffix.is_empty() {
            80
        } else {
            suffix
                .strip_prefix(':')
                .ok_or(NetError::Invalid("IPv6 port"))?
                .parse()
                .map_err(|_| NetError::Invalid("HTTP port"))?
        };
        return Ok((address, port));
    }
    if host.parse::<Ipv6Addr>().is_ok() {
        return Ok((host, 80));
    }
    match host.rsplit_once(':') {
        Some((name, port)) => Ok((
            name,
            port.parse().map_err(|_| NetError::Invalid("HTTP port"))?,
        )),
        None => Ok((host, 80)),
    }
}
#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use alloc::collections::VecDeque;
    #[test]
    fn ipv6_permissions_keep_address_and_port_separate() {
        assert_eq!(
            authority("[2001:db8::1]:8080").unwrap(),
            ("2001:db8::1", 8080)
        );
        assert_eq!(authority("2001:db8::1").unwrap(), ("2001:db8::1", 80));
        assert_eq!(authority("host:8080").unwrap(), ("host", 8080));
        for host in ["[::1", "[::1]x", "[::1]:oops"] {
            assert!(authority(host).is_err());
        }
    }
    struct ClockStep(core::cell::Cell<u64>);
    impl Clock for ClockStep {
        fn now_us(&self) -> Result<u64, ClockError> {
            let now = self.0.get() + 10_000;
            self.0.set(now);
            Ok(now)
        }
    }
    struct Router {
        frames: VecDeque<Vec<u8>>,
        probes: usize,
        /// The /64 prefix the router advertises.
        prefix: &'static str,
        /// Whether its advertisement names a DNS server (RDNSS).
        rdnss: bool,
    }
    fn router(prefix: &'static str, rdnss: bool) -> Router {
        Router {
            frames: VecDeque::new(),
            probes: 0,
            prefix,
            rdnss,
        }
    }
    impl Link for Router {
        fn mac(&self) -> stela_drivers::net::Mac {
            stela_drivers::net::Mac([2, 0, 0, 0, 0, 2])
        }
        fn transmit(&mut self, frame: &[u8]) -> Result<(), LinkError> {
            if frame.len() >= 62 && frame[12..14] == [0x86, 0xdd] && frame[20] == 58 {
                if frame[54] == 135 {
                    self.probes += 1;
                }
                if frame[54] == 133 {
                    self.frames
                        .push_back(advertisement_with(self.prefix, self.rdnss));
                }
            }
            Ok(())
        }
        fn receive(&mut self) -> Result<Option<Vec<u8>>, LinkError> {
            Ok(self.frames.pop_front())
        }
    }
    pub(crate) fn advertisement() -> Vec<u8> {
        advertisement_with("2001:db8:1234::", true)
    }
    /// A router advertisement of `prefix`/64, the router being the default
    /// route, with an RDNSS server in it when `rdnss`.
    fn advertisement_with(prefix: &str, rdnss: bool) -> Vec<u8> {
        let source: Ipv6Addr = "fe80::1".parse().unwrap();
        let destination: Ipv6Addr = "ff02::1".parse().unwrap();
        let prefix: Ipv6Addr = prefix.parse().unwrap();
        let dns: Ipv6Addr = "2001:db8:1234::53".parse().unwrap();
        let length: u16 = if rdnss { 72 } else { 48 };
        let mut frame = vec![0; 14 + 40 + usize::from(length)];
        frame[..6].copy_from_slice(&[0x33, 0x33, 0, 0, 0, 1]);
        frame[6..12].copy_from_slice(&[2, 0, 0, 0, 0, 1]);
        frame[12..14].copy_from_slice(&[0x86, 0xdd]);
        let ip = &mut frame[14..];
        ip[0] = 0x60;
        ip[4..6].copy_from_slice(&length.to_be_bytes());
        ip[6] = 58;
        ip[7] = 255;
        ip[8..24].copy_from_slice(&source.octets());
        ip[24..40].copy_from_slice(&destination.octets());
        let icmp = &mut ip[40..];
        icmp[0] = 134;
        icmp[4] = 64;
        icmp[6..8].copy_from_slice(&600u16.to_be_bytes());
        let pi = &mut icmp[16..48];
        pi[0] = 3;
        pi[1] = 4;
        pi[2] = 64;
        pi[3] = 0xc0;
        pi[4..8].copy_from_slice(&600u32.to_be_bytes());
        pi[8..12].copy_from_slice(&300u32.to_be_bytes());
        pi[16..32].copy_from_slice(&prefix.octets());
        if rdnss {
            let dns_option = &mut icmp[48..72];
            dns_option[0] = 25;
            dns_option[1] = 3;
            dns_option[4..8].copy_from_slice(&600u32.to_be_bytes());
            dns_option[8..24].copy_from_slice(&dns.octets());
        }
        let sum = rdnss::checksum(source, destination, icmp);
        icmp[2..4].copy_from_slice(&sum.to_be_bytes());
        frame
    }
    #[test]
    fn dad_wait_starts_after_the_solicitation_is_transmitted() {
        use alloc::rc::Rc;
        use core::cell::Cell;
        struct LogicalClock(Rc<Cell<u64>>);
        impl Clock for LogicalClock {
            fn now_us(&self) -> Result<u64, ClockError> {
                Ok(self.0.get())
            }
        }
        struct DelayedLink {
            now: Rc<Cell<u64>>,
            transmissions: usize,
        }
        impl Link for DelayedLink {
            fn mac(&self) -> stela_drivers::net::Mac {
                stela_drivers::net::Mac([2, 0, 0, 0, 0, 2])
            }
            fn transmit(&mut self, frame: &[u8]) -> Result<(), LinkError> {
                assert_eq!(frame[54], 135);
                self.transmissions += 1;
                // A real link may wait for its root/queue owner before the
                // solicitation is published, consuming the earlier deadline.
                self.now.set(self.now.get() + 2_000_000);
                Ok(())
            }
            fn receive(&mut self) -> Result<Option<Vec<u8>>, LinkError> {
                Ok(None)
            }
        }
        let now = Rc::new(Cell::new(0));
        let mut stack = Stack::new(
            DelayedLink {
                now: now.clone(),
                transmissions: 0,
            },
            LogicalClock(now.clone()),
            7,
        )
        .unwrap();
        stack.probe_ipv6().unwrap();
        assert_eq!(now.get(), 2_000_000);
        stack.probe_ipv6().unwrap();
        assert!(stack.phy.approved_ipv6.is_empty());
        now.set(2_999_999);
        stack.probe_ipv6().unwrap();
        assert!(stack.phy.approved_ipv6.is_empty());
        now.set(3_000_000);
        stack.probe_ipv6().unwrap();
        assert_eq!(stack.phy.approved_ipv6, [stack.phy.local_ipv6]);
        assert_eq!(stack.phy.link.transmissions, 1);
    }
    /// A link whose carrier comes up at `up_us` on a clock that advances
    /// 10 ms per reading, and that may take `wait_us` to bring it up.
    struct LateCarrier {
        now: alloc::rc::Rc<core::cell::Cell<u64>>,
        up_us: u64,
        wait_us: u64,
    }
    impl Link for LateCarrier {
        fn mac(&self) -> stela_drivers::net::Mac {
            stela_drivers::net::Mac([2, 0, 0, 0, 0, 2])
        }
        fn carrier(&mut self) -> bool {
            self.now.get() >= self.up_us
        }
        fn carrier_wait_us(&self) -> u64 {
            self.wait_us
        }
        fn transmit(&mut self, _: &[u8]) -> Result<(), LinkError> {
            Ok(())
        }
        fn receive(&mut self) -> Result<Option<Vec<u8>>, LinkError> {
            Ok(None)
        }
    }
    struct SharedStep(alloc::rc::Rc<core::cell::Cell<u64>>);
    impl Clock for SharedStep {
        fn now_us(&self) -> Result<u64, ClockError> {
            let now = self.0.get() + 10_000;
            self.0.set(now);
            Ok(now)
        }
    }
    fn late_carrier(up_us: u64, wait_us: u64) -> (Result<(), NetError>, Vec<u64>, u64) {
        let now = alloc::rc::Rc::new(core::cell::Cell::new(0));
        let link = LateCarrier {
            now: now.clone(),
            up_us,
            wait_us,
        };
        let mut stack = Stack::new(link, SharedStep(now.clone()), 7).unwrap();
        let start = now.get();
        let mut seconds = Vec::new();
        let result = stack.await_carrier(&mut |second| seconds.push(second));
        (result, seconds, now.get() - start)
    }
    #[test]
    fn a_wired_link_gets_ten_seconds_for_carrier_and_is_taken_when_it_comes() {
        // A 10GBASE-T link up after 7 s is used at once, each second told.
        let (result, seconds, elapsed) =
            late_carrier(7_000_000, stela_drivers::net::WIRED_CARRIER_WAIT_US);
        assert_eq!(result, Ok(()));
        assert_eq!(seconds, [1, 2, 3, 4, 5, 6]);
        assert!(elapsed < 7_000_000, "{elapsed}");
        // No cable: no carrier after the link's whole wait, never sooner.
        let (result, seconds, elapsed) = late_carrier(u64::MAX, 10_000_000);
        assert_eq!(result, Err(NetError::NoCarrier));
        assert_eq!(seconds.last(), Some(&9));
        assert!((10_000_000..10_100_000).contains(&elapsed), "{elapsed}");
        // A link that says it needs longer (Wi-Fi) is waited for longer.
        let (result, _, elapsed) = late_carrier(30_000_000, 45_000_000);
        assert_eq!(result, Ok(()));
        assert!((29_000_000..30_000_000).contains(&elapsed), "{elapsed}");
    }
    /// A link with carrier whose device is gone: every receive fails.
    struct Gone(alloc::rc::Rc<core::cell::Cell<u32>>);
    impl Link for Gone {
        fn mac(&self) -> stela_drivers::net::Mac {
            stela_drivers::net::Mac([2, 0, 0, 0, 0, 3])
        }
        fn transmit(&mut self, _: &[u8]) -> Result<(), LinkError> {
            Ok(())
        }
        fn receive(&mut self) -> Result<Option<Vec<u8>>, LinkError> {
            self.0.set(self.0.get() + 1);
            Err(LinkError::Adapter("device removed".into()))
        }
    }
    #[test]
    fn a_receive_error_means_the_device_is_gone_and_ends_the_setup_at_once() {
        let receives = alloc::rc::Rc::new(core::cell::Cell::new(0));
        let now = alloc::rc::Rc::new(core::cell::Cell::new(0));
        let mut stack = Stack::new(Gone(receives.clone()), SharedStep(now.clone()), 7).unwrap();
        let result = stack.automatic();
        assert_eq!(
            result.map(|_| ()),
            Err(NetError::Link("device removed".into()))
        );
        // One receive, not a dead link polled to the DHCP deadline.
        assert_eq!(receives.get(), 1);
        assert!(now.get() < 1_000_000, "{}", now.get());
    }
    #[test]
    fn addresses_and_servers_set_by_hand_replace_dhcp_and_advertised_dns() {
        let router = router("2001:db8:1234::", true);
        let mut stack = Stack::new(router, ClockStep(core::cell::Cell::new(0)), 7).unwrap();
        let address = core::net::Ipv4Addr::new(192, 168, 1, 20);
        stack
            .set_ipv4(address, 24, Some(core::net::Ipv4Addr::new(192, 168, 1, 1)))
            .unwrap();
        stack.set_dns(&["9.9.9.9".parse().unwrap()]).unwrap();
        let configuration = stack.automatic().unwrap();
        assert!(
            configuration
                .addresses
                .contains(&IpCidr::Ipv4(Ipv4Cidr::new(address, 24)))
        );
        // The router's advertised RDNSS server does not displace the fixed one.
        for _ in 0..200 {
            stack.poll().unwrap();
        }
        assert_eq!(
            stack.dns_servers,
            [IpAddress::Ipv4(core::net::Ipv4Addr::new(9, 9, 9, 9))]
        );
    }
    #[test]
    fn ipv6_only_link_uses_real_router_advertisement_and_dad() {
        let router = router("2001:db8:1234::", true);
        let mut stack = Stack::new(router, ClockStep(core::cell::Cell::new(0)), 7).unwrap();
        let configuration = stack.automatic().unwrap();
        assert!(stack.config().is_none());
        assert!(configuration.addresses.iter().any(|address|matches!(address,IpCidr::Ipv6(cidr) if !cidr.address().is_unicast_link_local())));
        assert!(
            configuration
                .dns
                .contains(&IpAddress::Ipv6("2001:db8:1234::53".parse().unwrap()))
        );
        assert!(stack.phy.link.probes >= 2);
        // A global address is taken at once, without waiting for DHCPv4.
        assert!(
            stack.clock.0.get() < DHCP_WAIT_US,
            "{}",
            stack.clock.0.get()
        );
    }
    #[test]
    fn a_unique_local_prefix_alone_waits_for_dhcpv4_and_is_taken_at_the_deadline() {
        let router = router("fd12:3456:789a::", true);
        let mut stack = Stack::new(router, ClockStep(core::cell::Cell::new(0)), 7).unwrap();
        let configuration = stack.automatic().unwrap();
        // No lease came: the ULA is taken, but only once the wait is over.
        assert!(
            stack.clock.0.get() >= DHCP_WAIT_US,
            "{}",
            stack.clock.0.get()
        );
        assert!(stack.config().is_none());
        assert!(stack.ipv6_ready() && !stack.ipv6_global_ready());
        assert!(
            configuration.addresses.iter().any(
                |address| matches!(address, IpCidr::Ipv6(cidr) if unique_local(cidr.address()))
            )
        );
        // A name's global address is not reached from the ULA; a site's is.
        let global: Ipv6Addr = "2001:db8::1".parse().unwrap();
        let site: Ipv6Addr = "fd12:3456:789a::1".parse().unwrap();
        assert!(!stack.address_usable(IpAddress::Ipv6(global)));
        assert!(stack.address_usable(IpAddress::Ipv6(site)));
    }
    /// A router whose advertisement names no DNS server, with no DHCPv4:
    /// the global address is taken only once the DHCPv4 wait is over, not
    /// the moment it passes DAD, and the log says lookups ask the public
    /// resolvers.
    #[test]
    fn ipv6_alone_without_a_dns_server_waits_for_dhcpv4() {
        let mut stack = Stack::new(
            router("2001:db8:1234::", false),
            ClockStep(core::cell::Cell::new(0)),
            7,
        )
        .unwrap();
        let configuration = stack.automatic().unwrap();
        assert!(configuration.dns.is_empty());
        assert!(stack.ipv6_native_ready());
        assert!(
            stack.clock.0.get() >= DHCP_WAIT_US,
            "{}",
            stack.clock.0.get()
        );
        let notes = stack.admission_notes();
        assert!(notes[0].starts_with("no IPv4 lease; dhcp: "), "{notes:?}");
        assert!(
            notes[1]
                .ends_with("lookups ask the public resolvers 2606:4700:4700::1111, 2620:fe::fe"),
            "{notes:?}"
        );
    }
    /// A 6to4 prefix, as a Windows PC sharing its connection advertises, is
    /// global but no reason to stop waiting for DHCPv4, even with a DNS
    /// server; a lookup then asks for IPv4 first where it has a lease.
    #[test]
    fn a_6to4_prefix_waits_for_dhcpv4_and_ipv4_is_preferred_to_it() {
        let mut stack = Stack::new(
            router("2002:c000:0201::", true),
            ClockStep(core::cell::Cell::new(0)),
            7,
        )
        .unwrap();
        let configuration = stack.automatic().unwrap();
        assert!(!configuration.dns.is_empty());
        assert!(stack.ipv6_global_ready() && !stack.ipv6_native_ready());
        assert!(
            stack.clock.0.get() >= DHCP_WAIT_US,
            "{}",
            stack.clock.0.get()
        );
        // IPv6 alone: the 6to4 address is what there is.
        assert_eq!(stack.query_kind(false), Some(DnsQueryType::Aaaa));
        stack
            .set_ipv4(
                core::net::Ipv4Addr::new(192, 168, 137, 20),
                24,
                Some(core::net::Ipv4Addr::new(192, 168, 137, 1)),
            )
            .unwrap();
        assert_eq!(stack.query_kind(false), Some(DnsQueryType::A));
        // A native prefix is still asked first.
        let mut native = Stack::new(
            router("2001:db8:1234::", true),
            ClockStep(core::cell::Cell::new(0)),
            7,
        )
        .unwrap();
        native.automatic().unwrap();
        native
            .set_ipv4(core::net::Ipv4Addr::new(192, 168, 1, 20), 24, None)
            .unwrap();
        assert_eq!(native.query_kind(false), Some(DnsQueryType::Aaaa));
        assert_eq!(native.query_kind(true), Some(DnsQueryType::A));
    }
    #[test]
    fn transition_addresses_are_6to4_and_teredo() {
        for (address, transition_address) in [
            ("2002:c000:201::1", true),
            ("2001:0:4136:e378::1", true),
            ("2001:db8::1", false),
            ("2001:1::1", false),
            ("2606:4700::1111", false),
            ("fd00::1", false),
        ] {
            assert_eq!(
                transition(address.parse().unwrap()),
                transition_address,
                "{address}"
            );
        }
    }
    #[test]
    fn unique_local_addresses_are_fc00_slash_7() {
        for (address, local) in [
            ("fc00::1", true),
            ("fd12:3456:789a::1", true),
            ("fe80::1", false),
            ("2001:db8::1", false),
            ("fbff::1", false),
        ] {
            assert_eq!(unique_local(address.parse().unwrap()), local, "{address}");
        }
    }
}
