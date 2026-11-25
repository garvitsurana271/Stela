//! What DNS replies said, seen on the wire beside smoltcp's resolver. Its
//! `Failed` is the same for a server that never answered, one that answered
//! SERVFAIL, REFUSED or NXDOMAIN, and a reply from an address other than
//! the server asked (which it drops), so a photo of a failed install could
//! not tell a dead DNS proxy from one that refused the name. A lookup's
//! failure now names which it was ([`Stack::lookup`]).
use super::*;

/// The last reply seen to one question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Answer {
    /// The question's name, in lower case, without a trailing dot.
    pub name: String,
    /// The question's type: 1 for A, 28 for AAAA.
    pub kind: u16,
    pub source: IpAddress,
    pub rcode: u8,
    /// The answer section's record count.
    pub records: u16,
    pub at: u64,
}

/// Questions whose last reply is kept.
const KEPT: usize = 8;

#[derive(Debug, Default)]
pub(super) struct Answers(Vec<Answer>);

impl Answers {
    /// Takes `frame` if it is a DNS reply (UDP from port 53).
    pub fn observe(&mut self, frame: &[u8], now: u64) {
        let Some(answer) = parse(frame, now) else {
            return;
        };
        self.0
            .retain(|kept| !(kept.name == answer.name && kept.kind == answer.kind));
        if self.0.len() >= KEPT {
            self.0.remove(0);
        }
        self.0.push(answer);
    }

    /// The last reply to `name`'s `kind` question that came at `since` or
    /// later.
    pub fn last(&self, name: &str, kind: u16, since: u64) -> Option<&Answer> {
        let name = normal(name);
        self.0
            .iter()
            .rev()
            .find(|answer| answer.name == name && answer.kind == kind && answer.at >= since)
    }
}

fn normal(name: &str) -> String {
    name.trim_end_matches('.').to_ascii_lowercase()
}

/// A DNS response code's name (RFC 1035 4.1.1, RFC 6895 2.3).
pub(super) fn rcode(code: u8) -> String {
    match code {
        0 => String::from("NOERROR"),
        1 => String::from("FORMERR"),
        2 => String::from("SERVFAIL"),
        3 => String::from("NXDOMAIN"),
        4 => String::from("NOTIMP"),
        5 => String::from("REFUSED"),
        other => alloc::format!("rcode {other}"),
    }
}

/// The question type of a lookup's query, as a reply carries it.
pub(super) fn kind(query: DnsQueryType) -> u16 {
    match query {
        DnsQueryType::Aaaa => 28,
        _ => 1,
    }
}

/// `frame`'s DNS reply: the source, the first question, the response code
/// and the answer count, for an unfragmented UDP datagram from port 53.
pub(super) fn parse(frame: &[u8], now: u64) -> Option<Answer> {
    let word = |bytes: &[u8], at: usize| -> Option<u16> {
        Some(u16::from_be_bytes([*bytes.get(at)?, *bytes.get(at + 1)?]))
    };
    let (source, udp) = match frame.get(12..14)? {
        [0x08, 0x00] => {
            let ip = frame.get(14..)?;
            let header = usize::from(ip.first()? & 0x0f) * 4;
            if ip[0] >> 4 != 4 || header < 20 || *ip.get(9)? != 17 {
                return None;
            }
            // A later fragment holds no UDP header; a first one, not the
            // whole reply.
            if word(ip, 6)? & 0x3fff != 0 {
                return None;
            }
            let total = usize::from(word(ip, 2)?).min(ip.len());
            let address: [u8; 4] = ip.get(12..16)?.try_into().ok()?;
            (
                IpAddress::Ipv4(core::net::Ipv4Addr::from(address)),
                ip.get(header..total)?,
            )
        }
        [0x86, 0xdd] => {
            let ip = frame.get(14..)?;
            if ip.first()? >> 4 != 6 || *ip.get(6)? != 17 {
                return None;
            }
            let length = usize::from(word(ip, 4)?);
            let address: [u8; 16] = ip.get(8..24)?.try_into().ok()?;
            let payload = ip.get(40..)?;
            (
                IpAddress::Ipv6(Ipv6Addr::from(address)),
                payload.get(..length.min(payload.len()))?,
            )
        }
        _ => return None,
    };
    if word(udp, 0)? != 53 {
        return None;
    }
    let dns = udp.get(8..)?;
    let flags = word(dns, 2)?;
    // A response to a standard query, with one question.
    if flags & 0x8000 == 0 || flags & 0x7800 != 0 || word(dns, 4)? != 1 {
        return None;
    }
    let records = word(dns, 6)?;
    let mut at = 12;
    let mut name = String::new();
    loop {
        let length = usize::from(*dns.get(at)?);
        at += 1;
        if length == 0 {
            break;
        }
        // A question's name is not compressed, and no label is longer.
        if length > 63 || name.len() + length > 254 {
            return None;
        }
        if !name.is_empty() {
            name.push('.');
        }
        name.extend(
            dns.get(at..at + length)?
                .iter()
                .map(|&byte| char::from(byte.to_ascii_lowercase())),
        );
        at += length;
    }
    Some(Answer {
        name,
        kind: word(dns, at)?,
        source,
        rcode: (flags & 0x000f) as u8,
        records,
        at: now,
    })
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A reply from `source` (IPv4) to the client, for `name`'s `kind`
    /// question, with `rcode` and `records` answers counted (none carried).
    pub(crate) fn reply(
        source: [u8; 4],
        name: &str,
        kind: u16,
        rcode: u8,
        records: u16,
    ) -> Vec<u8> {
        let mut dns = vec![0x12, 0x34, 0x81, 0x80 | rcode, 0, 1];
        dns.extend_from_slice(&records.to_be_bytes());
        dns.extend_from_slice(&[0, 0, 0, 0]);
        for label in name.split('.') {
            dns.push(label.len() as u8);
            dns.extend_from_slice(label.as_bytes());
        }
        dns.push(0);
        dns.extend_from_slice(&kind.to_be_bytes());
        dns.extend_from_slice(&[0, 1]);
        let mut udp = vec![0, 53, 0xc0, 0x01];
        udp.extend_from_slice(&((8 + dns.len()) as u16).to_be_bytes());
        udp.extend_from_slice(&[0, 0]);
        udp.extend_from_slice(&dns);
        let mut ip = vec![0x45, 0];
        ip.extend_from_slice(&((20 + udp.len()) as u16).to_be_bytes());
        ip.extend_from_slice(&[0, 0, 0x40, 0, 64, 17, 0, 0]);
        ip.extend_from_slice(&source);
        ip.extend_from_slice(&[10, 0, 2, 15]);
        ip.extend_from_slice(&udp);
        let mut frame = vec![2, 0, 0, 0, 0, 1, 2, 0, 0, 0, 0, 2, 0x08, 0x00];
        frame.extend_from_slice(&ip);
        // Ethernet's minimum frame pads a short one.
        frame.resize(frame.len().max(60), 0);
        frame
    }

    #[test]
    fn a_reply_gives_its_source_question_code_and_answer_count() {
        let frame = reply([192, 168, 137, 1], "Updates.CDN-Apple.com", 1, 2, 0);
        let answer = parse(&frame, 7).unwrap();
        assert_eq!(
            answer,
            Answer {
                name: String::from("updates.cdn-apple.com"),
                kind: 1,
                source: IpAddress::Ipv4(core::net::Ipv4Addr::new(192, 168, 137, 1)),
                rcode: 2,
                records: 0,
                at: 7,
            }
        );
        assert_eq!(rcode(answer.rcode), "SERVFAIL");
        assert_eq!(rcode(3), "NXDOMAIN");
        assert_eq!(rcode(5), "REFUSED");
        assert_eq!(rcode(9), "rcode 9");
        let mut answers = Answers::default();
        answers.observe(&frame, 7);
        assert!(answers.last("updates.cdn-apple.com.", 1, 7).is_some());
        // Not a reply to the AAAA question, nor one since a later query.
        assert!(answers.last("updates.cdn-apple.com", 28, 0).is_none());
        assert!(answers.last("updates.cdn-apple.com", 1, 8).is_none());
    }

    #[test]
    fn only_a_reply_from_port_53_is_taken() {
        let frame = reply([1, 1, 1, 1], "a.example", 1, 0, 1);
        let mut query = frame.clone();
        // A query (no response bit), a datagram from another port, and a
        // later fragment are not replies.
        query[14 + 20 + 8 + 2] &= 0x7f;
        assert!(parse(&query, 0).is_none());
        let mut other = frame.clone();
        other[14 + 20 + 1] = 54;
        assert!(parse(&other, 0).is_none());
        let mut fragment = frame.clone();
        fragment[14 + 7] = 1;
        assert!(parse(&fragment, 0).is_none());
        assert!(parse(&frame[..40], 0).is_none());
        assert!(parse(&frame, 0).is_some());
    }

    #[test]
    fn the_last_replies_are_kept_per_question() {
        let mut answers = Answers::default();
        for n in 0..KEPT + 2 {
            let name = alloc::format!("n{n}.example");
            answers.observe(&reply([1, 1, 1, 1], &name, 1, 0, 1), n as u64);
        }
        assert_eq!(answers.0.len(), KEPT);
        assert!(answers.last("n0.example", 1, 0).is_none());
        assert!(answers.last("n9.example", 1, 0).is_some());
        // A new reply to a question replaces its last.
        answers.observe(&reply([9, 9, 9, 9], "n9.example", 1, 3, 0), 20);
        let last = answers.last("n9.example", 1, 0).unwrap();
        assert_eq!((last.rcode, last.at), (3, 20));
        assert_eq!(answers.0.len(), KEPT);
    }
}
