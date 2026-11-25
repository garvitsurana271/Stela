//! The public-resolver fallback, and failures that tell a server that never
//! answered from one that answered with an error.
use super::*;
use smoltcp::socket::udp;

#[derive(Clone, Copy)]
enum Reply {
    Silent,
    Code(u8),
    Address(Ipv4),
}

/// The LAN's server, also answering DNS queries to each of its resolvers'
/// addresses as that one's [`Reply`] says, and noting which were asked.
struct Resolvers {
    lan: Lan,
    resolvers: Vec<(Ipv4, SocketHandle, Reply)>,
    asked: Vec<Ipv4>,
    signals: bool,
}

impl Resolvers {
    fn new(clock: TestClock, resolvers: &[(Ipv4, Reply)], signals: bool) -> Self {
        let mut lan = Lan::new(0, clock);
        let resolvers = resolvers
            .iter()
            .map(|&(address, reply)| {
                let prefix = if address.octets()[..3] == [10, 0, 2] {
                    24
                } else {
                    32
                };
                lan.iface.update_ip_addrs(|addresses| {
                    addresses.push(IpCidr::new(address.into(), prefix)).unwrap()
                });
                let mut socket = udp::Socket::new(
                    udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 16], vec![0; 8192]),
                    udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 16], vec![0; 8192]),
                );
                socket.bind((IpAddress::from(address), 53)).unwrap();
                (address, lan.sockets.add(socket), reply)
            })
            .collect();
        Self {
            lan,
            resolvers,
            asked: Vec::new(),
            signals,
        }
    }

    fn serve(&mut self) {
        for &(address, handle, reply) in &self.resolvers {
            let socket = self.lan.sockets.get_mut::<udp::Socket>(handle);
            while socket.can_recv() {
                let (query, meta) = socket.recv().unwrap();
                let query = query.to_vec();
                self.asked.push(address);
                let mut end = 12;
                while query[end] != 0 {
                    end += usize::from(query[end]) + 1;
                }
                let mut answer = query[..end + 5].to_vec();
                answer[8..12].fill(0);
                match reply {
                    Reply::Silent => continue,
                    Reply::Code(code) => {
                        answer[2..4].copy_from_slice(&(0x8180 | u16::from(code)).to_be_bytes());
                        answer[6..8].fill(0);
                    }
                    Reply::Address(to) => {
                        answer[2..4].copy_from_slice(&0x8180u16.to_be_bytes());
                        answer[6..8].copy_from_slice(&1u16.to_be_bytes());
                        answer.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
                        answer.extend_from_slice(&to.octets());
                    }
                }
                socket.send_slice(&answer, meta).unwrap();
            }
        }
    }
}

impl Link for Resolvers {
    fn mac(&self) -> Mac {
        CLIENT
    }
    fn transmit(&mut self, frame: &[u8]) -> Result<(), LinkError> {
        self.lan.transmit(frame)
    }
    fn receive(&mut self) -> Result<Option<Vec<u8>>, LinkError> {
        if self.lan.client.into.borrow().0.is_empty() {
            self.lan.step();
            self.serve();
            self.lan.step();
        }
        self.lan.client.receive()
    }
    fn signals(&self) -> bool {
        self.signals
    }
}

/// The LAN's clock, whose sleeps jump to the time asked for.
#[derive(Clone)]
struct Jumping(TestClock);

impl Clock for Jumping {
    fn now_us(&self) -> Result<u64, ClockError> {
        self.0.now_us()
    }
    fn sleep(&self, until_us: u64) -> Result<(), ClockError> {
        self.0.0.set(until_us.max(self.0.0.get()));
        Ok(())
    }
}

const NETWORK_DNS: Ipv4 = Ipv4::new(10, 0, 2, 3);
const CLOUDFLARE: Ipv4 = Ipv4::new(1, 1, 1, 1);
const QUAD9: Ipv4 = Ipv4::new(9, 9, 9, 9);
const FOUND: Ipv4 = Ipv4::new(10, 0, 2, 80);

/// A stack on the LAN with an IPv4 lease naming `dns`, its router being the
/// LAN's server (`router`) or none.
fn resolving(
    resolvers: &[(Ipv4, Reply)],
    dns: &[Ipv4],
    router: bool,
    signals: bool,
) -> Stack<Resolvers, Jumping> {
    let clock = TestClock::default();
    let link = Resolvers::new(clock.clone(), resolvers, signals);
    let mut stack = Stack::new(link, Jumping(clock), 7).unwrap();
    stack
        .set_config(Config {
            address: Ipv4Cidr::new(CLIENT_IP, 24),
            router: router.then_some(SERVER_IP),
            dns: dns.to_vec(),
        })
        .unwrap();
    stack
}

fn noted(stack: &Stack<Resolvers, Jumping>, part: &str) -> bool {
    stack.notes.iter().any(|note| note.contains(part))
}

#[test]
fn a_network_without_a_dns_server_asks_the_public_resolvers() {
    let mut s = resolving(
        &[
            (CLOUDFLARE, Reply::Address(FOUND)),
            (QUAD9, Reply::Address(FOUND)),
        ],
        &[],
        true,
        false,
    );
    assert_eq!(
        s.admission_notes(),
        [String::from(
            "no DNS server from DHCPv4 or IPv6 router advertisements: lookups ask the public resolvers 1.1.1.1, 9.9.9.9"
        )]
    );
    assert_eq!(s.resolve_now("cdn.example"), Ok(FOUND.into()));
    assert_eq!(s.phy.link.asked, [CLOUDFLARE]);
    assert!(
        noted(
            &s,
            "DNS cdn.example: no DNS server from DHCPv4 or IPv6 router advertisements; \
             asking the public resolvers 1.1.1.1, 9.9.9.9 instead (no --dns is set)"
        ),
        "{:?}",
        s.notes
    );
    assert!(
        noted(
            &s,
            "DNS cdn.example resolved by the public resolver 1.1.1.1"
        ),
        "{:?}",
        s.notes
    );
}

#[test]
fn a_server_error_is_named_and_the_public_resolvers_answer_in_its_place() {
    let mut s = resolving(
        &[
            (NETWORK_DNS, Reply::Code(2)),
            (CLOUDFLARE, Reply::Address(FOUND)),
        ],
        &[NETWORK_DNS],
        true,
        false,
    );
    assert!(s.admission_notes().is_empty());
    assert_eq!(s.resolve_now("cdn.example"), Ok(FOUND.into()));
    assert_eq!(s.phy.link.asked, [NETWORK_DNS, CLOUDFLARE]);
    assert!(
        noted(
            &s,
            "DNS cdn.example: 10.0.2.3 answered SERVFAIL with 0 answer records and no A address; \
             asking the public resolvers"
        ),
        "{:?}",
        s.notes
    );
}

#[test]
fn a_dns_server_set_by_hand_is_never_replaced() {
    let mut s = resolving(
        &[
            (NETWORK_DNS, Reply::Code(3)),
            (CLOUDFLARE, Reply::Address(FOUND)),
        ],
        &[],
        true,
        false,
    );
    s.set_dns(&[NETWORK_DNS.into()]).unwrap();
    assert!(s.admission_notes().is_empty());
    assert_eq!(
        s.resolve_now("cdn.example"),
        Err(NetError::Dns(String::from(
            "cdn.example: 10.0.2.3 answered NXDOMAIN with 0 answer records and no A address"
        )))
    );
    assert_eq!(s.phy.link.asked, [NETWORK_DNS]);
}

#[test]
fn servers_that_never_answer_are_told_from_one_that_answered() {
    let mut s = resolving(
        &[
            (NETWORK_DNS, Reply::Silent),
            (CLOUDFLARE, Reply::Silent),
            (QUAD9, Reply::Silent),
        ],
        &[NETWORK_DNS],
        true,
        true,
    );
    match s.resolve_now("cdn.example") {
        Err(NetError::Dns(why)) => {
            assert!(
                why.starts_with("cdn.example: no reply from 10.0.2.3 in "),
                "{why}"
            );
            assert!(
                why.contains(
                    " s to the A query; public resolvers: no reply from 1.1.1.1, 9.9.9.9 in "
                ),
                "{why}"
            );
            assert!(!why.contains("answered"), "{why}");
        }
        other => panic!("{other:?}"),
    }
    let asked = &s.phy.link.asked;
    for server in [NETWORK_DNS, CLOUDFLARE, QUAD9] {
        assert!(
            asked.contains(&server),
            "{server} was never asked: {asked:?}"
        );
    }
}

#[test]
fn without_a_server_or_a_route_the_failure_keeps_its_text() {
    let mut s = resolving(&[(CLOUDFLARE, Reply::Address(FOUND))], &[], false, false);
    assert_eq!(
        s.admission_notes(),
        [String::from(
            "no DNS server from DHCPv4 or IPv6 router advertisements, and no route to a public resolver"
        )]
    );
    assert_eq!(
        s.resolve_now("cdn.example"),
        Err(NetError::Dns(String::from(
            "no DNS server from DHCPv4 or IPv6 router advertisements"
        )))
    );
    assert!(s.phy.link.asked.is_empty());
}
