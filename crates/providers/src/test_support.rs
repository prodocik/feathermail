//! Shared fake IMAP server for this crate's own tests and, behind the
//! `test-support` feature, for `crates/service`'s end-to-end tests (T-082).
//!
//! This used to be a private `spawn_fake_server` inside `apply.rs`'s own
//! `#[cfg(test)] mod tests` -- the most complete of several ad-hoc fake
//! servers scattered across this crate's test modules (`sync_session.rs`,
//! `generic.rs`, `idle.rs`, `session.rs` each still have their own; T-082
//! deliberately leaves those alone, see `docs/plan.md`). It moved here,
//! largely unchanged, because `crates/service`'s tests need the exact same
//! stateful fake to prove `Core::dispatch(Command::Archive)` reaches a real
//! `UID MOVE` through the real `ImapMailProvider`/`RemoteLocator` -- and
//! `crates/service` cannot reach into `crates/providers`'s own `#[cfg(test)]`
//! code (that only exists for `feathermail-providers`'s own test binary).
//!
//! Gating: declared in `lib.rs` as
//! `#[cfg(any(test, feature = "test-support"))] pub mod test_support;` --
//! `cfg(test)` covers this crate's own unit tests (`apply.rs`'s, via an
//! import alias so none of its call sites changed), `feature =
//! "test-support"` covers external consumers that opt in explicitly. A
//! plain `cargo build -p feathermail-providers` enables neither, so this
//! module compiles into nothing for a normal build.
//!
//! What is new here, beyond the move: (1) the inner per-connection loop is
//! now wrapped in an outer `accept()` loop, so the server can serve more
//! than one sequential TCP connection over its lifetime (this crate's own
//! tests still only ever make one; T-082's test makes two -- a `LIST` walk,
//! then a later, separate connection for the apply session, mirroring
//! `crates/service/src/provider_factory.rs`'s "open a fresh connection when
//! work shows up" shape); (2) a `LIST` branch and a `tag_special_use`
//! setter, so a test can prove a mailbox is resolved through a real `LIST
//! SPECIAL-USE` walk rather than a hardcoded name.
//!
//! T-083 (third review round) adds one more thing: an `AUTHENTICATE
//! XOAUTH2` handler, gated by `FakeState::accept_xoauth2_token`, so a test
//! can script "reject this connection's saved access token, accept the
//! token a refresh call would issue" against a real IMAP handshake --
//! reusing the outer `accept()` loop above to serve the two sequential
//! connections a bounded reauth-and-retry actually opens (T-083's
//! `OauthReauth::reauthenticate` always dials a brand new
//! `ImapSession::connect` rather than reusing the rejected one). Mirrors
//! `xoauth2.rs`'s own `fake_server::spawn_imap` test double one-for-one
//! (same RFC 4959 initial-response decoding, same accept/reject shape),
//! which cannot serve this need itself: it only ever accepts a single
//! sequential connection, and lives in a private `#[cfg(test)]` module
//! `crates/service` cannot reach into (see this file's own doc comment
//! above for why `test_support` exists as a separate, shared module at
//! all).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Clone, Debug, Default)]
pub struct FakeMsg {
    pub uid: u32,
    pub flags: Vec<String>,
}

#[derive(Default)]
pub struct FakeState {
    pub mailboxes: HashMap<String, Vec<FakeMsg>>,
    next_uid: u32,
    special_use: HashMap<String, String>,
    /// How many `UID MOVE`/`UID COPY` commands this server has seen so
    /// far. End-state assertions alone ("message left the source mailbox,
    /// message is in the destination mailbox") cannot tell a real `UID
    /// MOVE` apart from the `UID COPY` + `UID STORE \Deleted` + `EXPUNGE`
    /// fallback -- both leave the same mailboxes behind. These counters
    /// let a test assert which wire command actually ran.
    pub uid_move_calls: u32,
    pub uid_copy_calls: u32,
    /// XOAUTH2 access token this server currently accepts (T-083). `None`
    /// (the default) means every `AUTHENTICATE XOAUTH2` attempt is
    /// rejected -- fail-shut, same as `special_use` having no entry for a
    /// mailbox means `LIST` reports no attribute for it, rather than any
    /// token being accepted by default.
    xoauth2_accept_token: Option<String>,
}

impl FakeState {
    /// Tags `mailbox` with an RFC 6154 SPECIAL-USE attribute (e.g.
    /// `"\\Archive"`) that this server's `LIST` response will report for
    /// it. Lets a test prove a destination mailbox was resolved through a
    /// real `LIST SPECIAL-USE` walk, not a hardcoded name -- the mailbox
    /// can (and, to make that a real test, should) be named nothing like
    /// its kind, e.g. `"Vault/Sealed"` tagged `\Archive`.
    pub fn tag_special_use(&mut self, mailbox: impl Into<String>, attr: impl Into<String>) {
        self.special_use.insert(mailbox.into(), attr.into());
    }

    /// Configures the one XOAUTH2 access token this server accepts
    /// (T-083): any `AUTHENTICATE XOAUTH2` attempt presenting a different
    /// token gets a `NO` before touching any other state. Applies to
    /// every connection the outer `accept()` loop serves for as long as
    /// this server runs -- a test that needs "reject the stale token on
    /// connection 1, accept the fresh one on connection 2" sets this
    /// *once*, up front, to the fresh token: the stale token a saved
    /// secret carries will simply never match it, on whichever connection
    /// it is tried on.
    pub fn accept_xoauth2_token(&mut self, token: impl Into<String>) {
        self.xoauth2_accept_token = Some(token.into());
    }
}

/// Spins up a minimal, stateful fake IMAP server on `127.0.0.1:0` (an
/// OS-assigned free port -- never a fixed one, so tests never collide or
/// flake on a busy port). Returns the port and a handle to the server's
/// mailbox state so a test can seed/inspect it while the server runs on
/// its own thread.
///
/// `initial` seeds mailboxes (name -> the UIDs already "in" it);
/// `move_supported` controls whether `CAPABILITY` advertises `MOVE` (so a
/// test can exercise `UID MOVE` directly, or the `UID COPY` + `UID STORE
/// \Deleted` + `EXPUNGE` fallback `ImapMailProvider` uses without it).
pub fn spawn_fake_imap_server(
    initial: Vec<(&'static str, Vec<u32>)>,
    move_supported: bool,
) -> (u16, Arc<Mutex<FakeState>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut mailboxes = HashMap::new();
    let mut max_uid = 0u32;
    for (name, uids) in initial {
        let msgs = uids
            .iter()
            .map(|&uid| {
                max_uid = max_uid.max(uid);
                FakeMsg {
                    uid,
                    flags: Vec::new(),
                }
            })
            .collect();
        mailboxes.insert(name.to_string(), msgs);
    }
    let state = Arc::new(Mutex::new(FakeState {
        mailboxes,
        next_uid: max_uid + 1,
        special_use: HashMap::new(),
        uid_move_calls: 0,
        uid_copy_calls: 0,
        xoauth2_accept_token: None,
    }));
    let state2 = state.clone();
    thread::spawn(move || {
        // Serves connections one at a time, sequentially -- never
        // concurrently, which is all any test here needs. A single-shot
        // `accept()` was enough while every test made exactly one
        // connection; this loops so a test may make a second, later
        // connection (e.g. a `LIST` walk followed by a separate apply
        // session) without the server having gone away.
        loop {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            write!(writer, "* OK fake ready\r\n").unwrap();
            writer.flush().unwrap();
            let mut selected: Option<String> = None;
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let trimmed = line.trim_end().to_string();
                let parts: Vec<&str> = trimmed.split_whitespace().collect();
                if parts.is_empty() {
                    continue;
                }
                let tag = parts[0];
                let upper = trimmed.to_ascii_uppercase();
                if upper.contains(" LOGIN ") {
                    write!(writer, "{tag} OK logged in\r\n").unwrap();
                } else if upper.contains("AUTHENTICATE XOAUTH2") {
                    // Initial-response form (RFC 4959): the base64 SASL
                    // blob is on this same line, never a separate `+`
                    // continuation round trip -- matches the client side
                    // (`wire.rs::imap_authenticate_xoauth2`) and mirrors
                    // `xoauth2.rs`'s own `fake_server::spawn_imap` double
                    // exactly (same indexing, same decode call), just
                    // wired into this module's `FakeState` instead of a
                    // fixed closed-over token.
                    let payload = parts.get(3).copied().unwrap_or("");
                    let decoded = base64::Engine::decode(
                        &base64::engine::general_purpose::STANDARD,
                        payload.trim(),
                    )
                    .unwrap_or_default();
                    let text = String::from_utf8_lossy(&decoded);
                    let accepted = state2
                        .lock()
                        .unwrap()
                        .xoauth2_accept_token
                        .as_deref()
                        .is_some_and(|expected| text.contains(&format!("auth=Bearer {expected}")));
                    if accepted {
                        write!(writer, "{tag} OK Success\r\n").unwrap();
                    } else {
                        write!(
                            writer,
                            "{tag} NO [AUTHENTICATIONFAILED] Invalid credentials\r\n"
                        )
                        .unwrap();
                    }
                } else if upper.contains(" CAPABILITY") {
                    if move_supported {
                        write!(writer, "* CAPABILITY IMAP4rev1 UIDPLUS MOVE\r\n{tag} OK CAPABILITY completed\r\n").unwrap();
                    } else {
                        write!(
                            writer,
                            "* CAPABILITY IMAP4rev1 UIDPLUS\r\n{tag} OK CAPABILITY completed\r\n"
                        )
                        .unwrap();
                    }
                } else if upper.contains(" LIST ") {
                    let st = state2.lock().unwrap();
                    let mut names: Vec<&String> = st.mailboxes.keys().collect();
                    names.sort();
                    for name in names {
                        let attrs = st.special_use.get(name).cloned().unwrap_or_default();
                        write!(writer, "* LIST ({attrs}) \"/\" \"{name}\"\r\n").unwrap();
                    }
                    drop(st);
                    write!(writer, "{tag} OK LIST completed\r\n").unwrap();
                } else if upper.contains(" SELECT ") {
                    let name = quoted_or_last(&trimmed);
                    let mut st = state2.lock().unwrap();
                    let count = st.mailboxes.entry(name.clone()).or_default().len();
                    let next = st.next_uid;
                    selected = Some(name);
                    write!(writer, "* {count} EXISTS\r\n").unwrap();
                    write!(writer, "* OK [UIDVALIDITY 1] ok\r\n").unwrap();
                    write!(writer, "* OK [UIDNEXT {next}] ok\r\n").unwrap();
                    write!(writer, "{tag} OK [READ-WRITE] SELECT completed\r\n").unwrap();
                } else if upper.contains(" APPEND ") {
                    let mailbox = quoted_or_last(&trimmed);
                    let flags = flag_from(&trimmed);
                    let bytes = literal_len_from(&trimmed)
                        .expect("fake APPEND command must declare a literal length");
                    write!(writer, "+ send literal\r\n").unwrap();
                    writer.flush().unwrap();
                    let mut literal = vec![0_u8; bytes + 2];
                    reader.read_exact(&mut literal).unwrap();
                    assert_eq!(
                        &literal[bytes..],
                        b"\r\n",
                        "IMAP APPEND literal must be terminated exactly once"
                    );
                    let mut st = state2.lock().unwrap();
                    let uid = st.next_uid;
                    st.next_uid += 1;
                    st.mailboxes.entry(mailbox).or_default().push(FakeMsg {
                        uid,
                        flags: flags
                            .split_whitespace()
                            .filter(|flag| !flag.is_empty())
                            .map(str::to_string)
                            .collect(),
                    });
                    write!(writer, "{tag} OK [APPENDUID 1 {uid}] APPEND completed\r\n").unwrap();
                } else if upper.contains("UID STORE") {
                    let uids = uid_set_from(&parts, "STORE");
                    let add = upper.contains("+FLAGS");
                    let flag = flag_from(&trimmed);
                    let mbox = selected.clone().unwrap_or_default();
                    let mut st = state2.lock().unwrap();
                    if let Some(msgs) = st.mailboxes.get_mut(&mbox) {
                        for m in msgs.iter_mut().filter(|m| uids.contains(&m.uid)) {
                            if add {
                                if !m.flags.iter().any(|f| f == &flag) {
                                    m.flags.push(flag.clone());
                                }
                            } else {
                                m.flags.retain(|f| f != &flag);
                            }
                        }
                    }
                    write!(writer, "{tag} OK UID STORE completed\r\n").unwrap();
                } else if upper.contains("UID COPY") {
                    let uids = uid_set_from(&parts, "COPY");
                    let dest = quoted_or_last(&trimmed);
                    let mbox = selected.clone().unwrap_or_default();
                    let mut st = state2.lock().unwrap();
                    let to_copy: Vec<FakeMsg> = st
                        .mailboxes
                        .get(&mbox)
                        .map(|msgs| {
                            msgs.iter()
                                .filter(|m| uids.contains(&m.uid))
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default();
                    let new_uids: Vec<u32> = to_copy
                        .iter()
                        .map(|_| {
                            let uid = st.next_uid;
                            st.next_uid += 1;
                            uid
                        })
                        .collect();
                    {
                        let dest_entry = st.mailboxes.entry(dest).or_default();
                        for (uid, src) in new_uids.into_iter().zip(to_copy.iter()) {
                            dest_entry.push(FakeMsg {
                                uid,
                                flags: src.flags.clone(),
                            });
                        }
                    }
                    st.uid_copy_calls += 1;
                    write!(writer, "{tag} OK UID COPY completed\r\n").unwrap();
                } else if move_supported && upper.contains("UID MOVE") {
                    let uids = uid_set_from(&parts, "MOVE");
                    let dest = quoted_or_last(&trimmed);
                    let mbox = selected.clone().unwrap_or_default();
                    let mut st = state2.lock().unwrap();
                    let to_move: Vec<FakeMsg> = st
                        .mailboxes
                        .get(&mbox)
                        .map(|msgs| {
                            msgs.iter()
                                .filter(|m| uids.contains(&m.uid))
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default();
                    let new_uids: Vec<u32> = to_move
                        .iter()
                        .map(|_| {
                            let uid = st.next_uid;
                            st.next_uid += 1;
                            uid
                        })
                        .collect();
                    {
                        let dest_entry = st.mailboxes.entry(dest).or_default();
                        for (uid, src) in new_uids.into_iter().zip(to_move.iter()) {
                            dest_entry.push(FakeMsg {
                                uid,
                                flags: src.flags.clone(),
                            });
                        }
                    }
                    if let Some(msgs) = st.mailboxes.get_mut(&mbox) {
                        msgs.retain(|m| !uids.contains(&m.uid));
                    }
                    st.uid_move_calls += 1;
                    write!(writer, "{tag} OK UID MOVE completed\r\n").unwrap();
                } else if upper.contains(" RENAME ") {
                    // T-060t: RENAME moves a mailbox and everything in it.
                    // The fake keeps the messages so a test can prove the
                    // mail travelled with the name rather than vanishing.
                    let names = quoted_arguments(&trimmed);
                    let mut st = state2.lock().unwrap();
                    match (names.first(), names.get(1)) {
                        (Some(from), Some(to)) if st.mailboxes.contains_key(to) => {
                            write!(writer, "{tag} NO [ALREADYEXISTS] Mailbox exists\r\n").unwrap();
                            let _ = from;
                        }
                        (Some(from), Some(to)) => match st.mailboxes.remove(from) {
                            Some(msgs) => {
                                st.mailboxes.insert(to.clone(), msgs);
                                write!(writer, "{tag} OK RENAME completed\r\n").unwrap();
                            }
                            None => {
                                write!(writer, "{tag} NO [NONEXISTENT] No such mailbox\r\n")
                                    .unwrap();
                            }
                        },
                        _ => {
                            write!(writer, "{tag} BAD RENAME needs two mailbox names\r\n").unwrap();
                        }
                    }
                } else if upper.contains(" CREATE ") {
                    let name = quoted_or_last(&trimmed);
                    let mut st = state2.lock().unwrap();
                    match st.mailboxes.entry(name) {
                        std::collections::hash_map::Entry::Occupied(_) => {
                            write!(
                                writer,
                                "{tag} NO [ALREADYEXISTS] Mailbox already exists\r\n"
                            )
                            .unwrap();
                        }
                        std::collections::hash_map::Entry::Vacant(e) => {
                            e.insert(Vec::new());
                            write!(writer, "{tag} OK CREATE completed\r\n").unwrap();
                        }
                    }
                } else if upper.contains(" DELETE ") {
                    // T-060u: RFC 3501's DELETE destroys the mailbox *and*
                    // everything in it. This fake models exactly that --
                    // the permissive server, not a cautious one that
                    // refuses a non-empty mailbox -- so that a test proving
                    // mail survived is proving the *applier's* own
                    // emptiness guard, not the server's good manners.
                    let name = quoted_or_last(&trimmed);
                    let mut st = state2.lock().unwrap();
                    match st.mailboxes.remove(&name) {
                        Some(_) => {
                            write!(writer, "{tag} OK DELETE completed\r\n").unwrap();
                        }
                        None => {
                            write!(writer, "{tag} NO [NONEXISTENT] No such mailbox\r\n").unwrap();
                        }
                    }
                } else if upper.contains("UID EXPUNGE") {
                    let uids = uid_set_from(&parts, "EXPUNGE");
                    let mbox = selected.clone().unwrap_or_default();
                    let mut st = state2.lock().unwrap();
                    if let Some(msgs) = st.mailboxes.get_mut(&mbox) {
                        msgs.retain(|m| {
                            !(uids.contains(&m.uid) && m.flags.iter().any(|f| f == "\\Deleted"))
                        });
                    }
                    write!(writer, "{tag} OK UID EXPUNGE completed\r\n").unwrap();
                } else if upper.contains("EXPUNGE") {
                    let mbox = selected.clone().unwrap_or_default();
                    let mut st = state2.lock().unwrap();
                    if let Some(msgs) = st.mailboxes.get_mut(&mbox) {
                        msgs.retain(|m| !m.flags.iter().any(|f| f == "\\Deleted"));
                    }
                    write!(writer, "{tag} OK EXPUNGE completed\r\n").unwrap();
                } else if upper.contains("LOGOUT") {
                    write!(writer, "* BYE\r\n{tag} OK LOGOUT\r\n").unwrap();
                    break;
                } else {
                    write!(writer, "{tag} BAD unknown command\r\n").unwrap();
                }
                writer.flush().unwrap();
            }
            // Inner loop ended on EOF or LOGOUT -- go back to `accept()`
            // for a possible next connection instead of ending the thread.
        }
    });
    (port, state)
}

/// Every double-quoted argument on a command line, in order. `RENAME` is
/// the first command here that takes two mailbox names, and
/// [`quoted_or_last`] can only ever return one.
fn quoted_arguments(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find('"') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('"') else { break };
        out.push(after[..end].to_string());
        rest = &after[end + 1..];
    }
    out
}

fn quoted_or_last(line: &str) -> String {
    if let Some(start) = line.find('"') {
        if let Some(end) = line[start + 1..].find('"') {
            return line[start + 1..start + 1 + end].to_string();
        }
    }
    line.split_whitespace()
        .last()
        .unwrap_or_default()
        .to_string()
}

fn uid_set_from(parts: &[&str], keyword: &str) -> Vec<u32> {
    let Some(idx) = parts.iter().position(|p| p.eq_ignore_ascii_case(keyword)) else {
        return Vec::new();
    };
    let Some(set) = parts.get(idx + 1) else {
        return Vec::new();
    };
    set.split(',')
        .filter_map(|s| s.parse::<u32>().ok())
        .collect()
}

fn flag_from(line: &str) -> String {
    if let Some(start) = line.find('(') {
        if let Some(end) = line[start..].find(')') {
            return line[start + 1..start + end].trim().to_string();
        }
    }
    String::new()
}

fn literal_len_from(line: &str) -> Option<usize> {
    let start = line.rfind('{')? + 1;
    let end = line[start..].find('}')? + start;
    line[start..end].parse().ok()
}
