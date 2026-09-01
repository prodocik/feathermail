//! D20: Mozilla ISPDB + SRV `_imaps` / `_submission`. Manual form is T-017.

use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

use feathermail_core::{MailSecurity, MailboxForm};
use simple_dns::rdata::RData;
use simple_dns::{Name, Packet, PacketFlag, Question, CLASS, TYPE};

const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const DNS_TIMEOUT: Duration = Duration::from_secs(3);
const ISPDB: &str = "https://autoconfig.thunderbird.net/v1.1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoconfigError {
    pub message: String,
}

impl AutoconfigError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

pub trait HttpGet {
    fn get(&self, url: &str) -> Result<Option<String>, AutoconfigError>;
}

pub trait DnsSrv {
    fn srv(&self, name: &str) -> Result<Vec<SrvRecord>, AutoconfigError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SrvRecord {
    pub priority: u16,
    pub port: u16,
    pub target: String,
}

pub struct LiveHttp;
pub struct LiveDns;

impl HttpGet for LiveHttp {
    fn get(&self, url: &str) -> Result<Option<String>, AutoconfigError> {
        let agent = ureq::builder().timeout(HTTP_TIMEOUT).build();
        match agent.get(url).call() {
            Ok(resp) => {
                let body = resp
                    .into_string()
                    .map_err(|e| AutoconfigError::new(e.to_string()))?;
                Ok(Some(body))
            }
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(e) => Err(AutoconfigError::new(e.to_string())),
        }
    }
}

impl DnsSrv for LiveDns {
    fn srv(&self, name: &str) -> Result<Vec<SrvRecord>, AutoconfigError> {
        dns_srv(name, nameserver())
    }
}

/// Look up IMAP/SMTP for `email`. `None` means use the manual form.
pub fn lookup(
    email: &str,
    http: &impl HttpGet,
    dns: &impl DnsSrv,
) -> Result<Option<MailboxForm>, AutoconfigError> {
    let domain = email
        .rsplit_once('@')
        .map(|(_, d)| d.trim().trim_end_matches('.').to_ascii_lowercase())
        .filter(|d| !d.is_empty())
        .ok_or_else(|| AutoconfigError::new("Enter an email address."))?;

    let mut form = from_ispdb(email, &domain, http)?;
    fill_srv(&mut form, email, &domain, dns)?;
    match form {
        Some(form)
            if !form.imap_host.is_empty()
                && !form.smtp_host.is_empty()
                && !form.email.is_empty() =>
        {
            Ok(Some(form))
        }
        _ => Ok(None),
    }
}

fn from_ispdb(
    email: &str,
    domain: &str,
    http: &impl HttpGet,
) -> Result<Option<MailboxForm>, AutoconfigError> {
    let url = format!("{ISPDB}/{domain}");
    let Some(xml) = http.get(&url)? else {
        return Ok(None);
    };
    Ok(parse_ispdb(email, &xml))
}

pub fn parse_ispdb(email: &str, xml: &str) -> Option<MailboxForm> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let incoming = doc.descendants().find(|n| {
        n.has_tag_name("incomingServer")
            && n.attribute("type")
                .is_some_and(|t| t.eq_ignore_ascii_case("imap"))
    })?;
    let outgoing = doc.descendants().find(|n| {
        n.has_tag_name("outgoingServer")
            && n.attribute("type")
                .is_some_and(|t| t.eq_ignore_ascii_case("smtp"))
    })?;
    let imap_host = child_text(&incoming, "hostname")?;
    let smtp_host = child_text(&outgoing, "hostname")?;
    let imap_security = socket_type(child_text(&incoming, "socketType").as_deref());
    let smtp_security = socket_type(child_text(&outgoing, "socketType").as_deref());
    let imap_port = child_text(&incoming, "port")
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| imap_security.default_imap_port());
    let smtp_port = child_text(&outgoing, "port")
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| smtp_security.default_smtp_port());
    Some(MailboxForm {
        email: email.trim().to_string(),
        imap_host,
        imap_port,
        imap_security,
        smtp_host,
        smtp_port,
        smtp_security,
    })
}

fn fill_srv(
    form: &mut Option<MailboxForm>,
    email: &str,
    domain: &str,
    dns: &impl DnsSrv,
) -> Result<(), AutoconfigError> {
    let imaps = dns.srv(&format!("_imaps._tcp.{domain}"))?;
    let submission = dns.srv(&format!("_submission._tcp.{domain}"))?;
    if form.is_none() && (imaps.is_empty() && submission.is_empty()) {
        return Ok(());
    }
    let slot = form.get_or_insert_with(|| MailboxForm {
        email: email.trim().to_string(),
        imap_host: String::new(),
        imap_port: MailSecurity::Ssl.default_imap_port(),
        imap_security: MailSecurity::Ssl,
        smtp_host: String::new(),
        smtp_port: MailSecurity::StartTls.default_smtp_port(),
        smtp_security: MailSecurity::StartTls,
    });
    if slot.imap_host.is_empty() {
        if let Some(rec) = pick_srv(&imaps) {
            slot.imap_host = rec.target.trim_end_matches('.').to_string();
            slot.imap_port = rec.port;
            slot.imap_security = MailSecurity::Ssl;
        }
    }
    if slot.smtp_host.is_empty() {
        if let Some(rec) = pick_srv(&submission) {
            slot.smtp_host = rec.target.trim_end_matches('.').to_string();
            slot.smtp_port = rec.port;
            slot.smtp_security = MailSecurity::StartTls;
        }
    }
    Ok(())
}

fn pick_srv(records: &[SrvRecord]) -> Option<&SrvRecord> {
    records.iter().min_by_key(|r| r.priority)
}

fn child_text(node: &roxmltree::Node<'_, '_>, tag: &str) -> Option<String> {
    node.children()
        .find(|n| n.has_tag_name(tag))
        .and_then(|n| n.text())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn socket_type(raw: Option<&str>) -> MailSecurity {
    match raw.map(str::to_ascii_uppercase).as_deref() {
        Some("SSL") | Some("TLS") => MailSecurity::Ssl,
        Some("STARTTLS") => MailSecurity::StartTls,
        Some("PLAIN") | Some("NONE") => MailSecurity::None,
        _ => MailSecurity::Ssl,
    }
}

fn nameserver() -> SocketAddr {
    if let Ok(text) = std::fs::read_to_string("/etc/resolv.conf") {
        for line in text.lines() {
            let line = line.trim();
            if let Some(ip) = line.strip_prefix("nameserver ") {
                if let Ok(addr) = format!("{}:53", ip.trim()).parse() {
                    return addr;
                }
            }
        }
    }
    "1.1.1.1:53".parse().expect("static")
}

fn dns_srv(name: &str, ns: SocketAddr) -> Result<Vec<SrvRecord>, AutoconfigError> {
    let qname = Name::new(name).map_err(|e| AutoconfigError::new(e.to_string()))?;
    let mut packet = Packet::new_query(0x5e1f);
    packet.set_flags(PacketFlag::RECURSION_DESIRED);
    packet.questions.push(Question::new(
        qname,
        TYPE::SRV.into(),
        CLASS::IN.into(),
        false,
    ));
    let bytes = packet
        .build_bytes_vec_compressed()
        .map_err(|e| AutoconfigError::new(e.to_string()))?;
    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| AutoconfigError::new(e.to_string()))?;
    sock.set_read_timeout(Some(DNS_TIMEOUT))
        .map_err(|e| AutoconfigError::new(e.to_string()))?;
    sock.send_to(&bytes, ns)
        .map_err(|e| AutoconfigError::new(e.to_string()))?;
    let mut buf = [0u8; 512];
    let (n, _) = sock
        .recv_from(&mut buf)
        .map_err(|e| AutoconfigError::new(e.to_string()))?;
    let packet = Packet::parse(&buf[..n]).map_err(|e| AutoconfigError::new(e.to_string()))?;
    let mut out = Vec::new();
    for ans in packet.answers {
        if let RData::SRV(srv) = ans.rdata {
            out.push(SrvRecord {
                priority: srv.priority,
                port: srv.port,
                target: srv.target.to_string(),
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct MapHttp(HashMap<String, String>);
    struct MapDns(HashMap<String, Vec<SrvRecord>>);

    impl HttpGet for MapHttp {
        fn get(&self, url: &str) -> Result<Option<String>, AutoconfigError> {
            Ok(self.0.get(url).cloned())
        }
    }

    impl DnsSrv for MapDns {
        fn srv(&self, name: &str) -> Result<Vec<SrvRecord>, AutoconfigError> {
            Ok(self.0.get(name).cloned().unwrap_or_default())
        }
    }

    const XML: &str = r#"
<clientConfig>
  <emailProvider>
    <incomingServer type="imap">
      <hostname>imap.example.com</hostname>
      <port>993</port>
      <socketType>SSL</socketType>
    </incomingServer>
    <outgoingServer type="smtp">
      <hostname>smtp.example.com</hostname>
      <port>587</port>
      <socketType>STARTTLS</socketType>
    </outgoingServer>
  </emailProvider>
</clientConfig>
"#;

    #[test]
    fn ispdb_xml_fills_form() {
        let form = parse_ispdb("you@example.com", XML).unwrap();
        assert_eq!(form.imap_host, "imap.example.com");
        assert_eq!(form.imap_port, 993);
        assert_eq!(form.imap_security, MailSecurity::Ssl);
        assert_eq!(form.smtp_host, "smtp.example.com");
        assert_eq!(form.smtp_port, 587);
        assert_eq!(form.smtp_security, MailSecurity::StartTls);
        assert!(!format!("{form:?}").contains("password"));
    }

    #[test]
    fn lookup_prefers_ispdb() {
        let mut http = HashMap::new();
        http.insert(
            "https://autoconfig.thunderbird.net/v1.1/example.com".into(),
            XML.into(),
        );
        let form = lookup("you@example.com", &MapHttp(http), &MapDns(HashMap::new()))
            .unwrap()
            .unwrap();
        assert_eq!(form.imap_host, "imap.example.com");
    }

    #[test]
    fn lookup_falls_back_to_srv() {
        let mut dns = HashMap::new();
        dns.insert(
            "_imaps._tcp.example.com".into(),
            vec![SrvRecord {
                priority: 0,
                port: 993,
                target: "mail.example.com.".into(),
            }],
        );
        dns.insert(
            "_submission._tcp.example.com".into(),
            vec![SrvRecord {
                priority: 10,
                port: 587,
                target: "smtp.example.com.".into(),
            }],
        );
        let form = lookup("you@example.com", &MapHttp(HashMap::new()), &MapDns(dns))
            .unwrap()
            .unwrap();
        assert_eq!(form.imap_host, "mail.example.com");
        assert_eq!(form.imap_security, MailSecurity::Ssl);
        assert_eq!(form.smtp_host, "smtp.example.com");
        assert_eq!(form.smtp_security, MailSecurity::StartTls);
    }

    #[test]
    fn unknown_domain_is_none() {
        let got = lookup(
            "you@no-such.example",
            &MapHttp(HashMap::new()),
            &MapDns(HashMap::new()),
        )
        .unwrap();
        assert!(got.is_none());
    }
}
