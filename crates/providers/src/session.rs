//! Live IMAP session for incremental sync (T-022, first half). Thin sync
//! layer over [`crate::wire`]; the tokio glue and SQLite binding are the
//! second half of T-022 (out of scope here, D9).
//!
//! Auth reuses the existing wire helpers ([`wire::imap_login`],
//! [`wire::imap_authenticate_xoauth2`]) instead of duplicating LOGIN/XOAUTH2
//! logic that lives in `generic.rs` / `gmail.rs`.
//!
//! The response parser is minimal and defensive: IMAP literals (`{n}`) are
//! read as exact byte spans (so header text and non-ASCII folder names never
//! get mangled by naive line splitting), and any malformed or truncated
//! response turns into a [`ConnectError`], never a panic.

use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use feathermail_attachments::{decode_to_file, TransferEncoding};
use feathermail_core::{ConnectError, FolderKind, MailboxForm};

use crate::wire::{self, ImapStream};

/// How to authenticate the session (D18/D19): password LOGIN for generic
/// IMAP, XOAUTH2 for Gmail/Microsoft.
pub enum ImapAuth {
    Login(String),
    XOauth2(String),
}

/// Server capability flags relevant to sync (D30, D33): CONDSTORE/QRESYNC for
/// modseq-based flag deltas, UIDPLUS for APPEND feedback, IDLE, MOVE.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub condstore: bool,
    pub qresync: bool,
    pub uidplus: bool,
    pub idle: bool,
    pub can_move: bool,
    /// Gmail IMAP extensions (`X-GM-EXT-1`): `X-GM-THRID` / `X-GM-LABELS` /
    /// `X-GM-RAW`. FETCH must not name `X-GM-THRID` unless this is set —
    /// generic IMAP answers BAD.
    pub x_gm_ext1: bool,
    pub raw: Vec<String>,
}

impl Capabilities {
    fn from_raw(raw: Vec<String>) -> Self {
        let upper: Vec<String> = raw.iter().map(|c| c.to_ascii_uppercase()).collect();
        let has = |name: &str| upper.iter().any(|c| c == name);
        Self {
            condstore: has("CONDSTORE"),
            qresync: has("QRESYNC"),
            uidplus: has("UIDPLUS"),
            idle: has("IDLE"),
            can_move: has("MOVE"),
            x_gm_ext1: has("X-GM-EXT-1"),
            raw,
        }
    }
}

/// One `LIST` entry, mapped toward [`FolderKind`] via SPECIAL-USE (RFC 6154)
/// or the `INBOX` name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderListing {
    pub name: String,
    pub delimiter: Option<char>,
    pub has_children: bool,
    pub no_select: bool,
    pub kind: FolderKind,
}

/// Result of `SELECT` (D33): the four numbers the sync engine needs to
/// decide first-run vs. incremental vs. UIDVALIDITY invalidation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectedMailbox {
    pub uidvalidity: u32,
    pub uidnext: u32,
    pub exists: u32,
    pub highest_modseq: Option<u64>,
}

/// A UID range for `UID FETCH`. `to: None` renders as the IMAP `*` (open
/// upper bound, "up to whatever exists now").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UidRange {
    pub from: u32,
    pub to: Option<u32>,
}

impl UidRange {
    pub fn bounded(from: u32, to: u32) -> Self {
        Self { from, to: Some(to) }
    }

    pub fn open(from: u32) -> Self {
        Self { from, to: None }
    }

    fn to_wire(self) -> String {
        match self.to {
            Some(to) => format!("{}:{}", self.from, to),
            None => format!("{}:*", self.from),
        }
    }
}

/// Metadata for one message (T-022: headers only, no bodies — T-024 fetches
/// those lazily). Also reused for flags-only responses
/// ([`ImapSession::uid_fetch_flags_changed_since`]), where every field but
/// `uid`/`flags` is left at its default.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HeaderMeta {
    pub uid: u32,
    pub flags: Vec<String>,
    pub internaldate: Option<String>,
    pub size_bytes: Option<u64>,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub cc: Option<String>,
    pub subject: Option<String>,
    pub date: Option<String>,
    /// Present only when the FETCH asked for `X-GM-THRID` (Gmail).
    pub gm_thrid: Option<String>,
}

const MAX_LITERAL_BYTES: usize = 2 * 1024 * 1024;
const MAX_RESPONSE_LINES: usize = 8192;
/// Bound for a whole-message `BODY.PEEK[]` literal (T-024) -- much larger
/// than [`MAX_LITERAL_BYTES`], which only ever bounds the short header
/// blob `uid_fetch_headers` reads. A full message (headers + text +
/// whatever MIME parts ride along) can legitimately run tens of MB; this
/// still bounds it so a broken or hostile server claiming an absurd
/// literal length can't make us allocate without limit.
const MAX_BODY_LITERAL_BYTES: usize = 64 * 1024 * 1024;

/// Ceiling on what one batched `UID FETCH` keeps in memory at once.
///
/// A batch holds every body it collected until the caller stores them, so
/// without a ceiling twenty large messages would be twenty large messages
/// resident at the same time -- against a project that has an explicit RAM
/// target (D4). Past this the remaining literals are still read off the
/// socket (the stream is shared and has to be drained) but not kept: those
/// messages simply come back unwarmed, and the one that the reader
/// actually opens is fetched on its own through the single-message path.
const MAX_BATCH_BODY_BYTES: usize = 24 * 1024 * 1024;

/// Limits for one attachment section fetch. Both bounds are deliberate:
/// transfer encodings can inflate a MIME part on the wire, while malformed
/// input might otherwise contain arbitrarily much data without producing many
/// decoded bytes. Neither limit is a whole-message allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachmentFetchLimits {
    pub max_wire_bytes: u64,
    pub max_decoded_bytes: u64,
}

/// A live IMAP connection plus the handful of commands the sync engine
/// needs. Synchronous by design (T-022 first half); async/tokio wrapping is
/// the second half.
pub struct ImapSession {
    stream: ImapStream,
    tag_seq: u32,
    /// Tag of the currently outstanding `IDLE`, if one is active — set by
    /// [`Self::idle_start`], consumed by [`Self::idle_done`] (T-026).
    idle_tag: Option<String>,
    /// Bytes read so far toward the next IDLE untagged line whose
    /// terminating `\n` hasn't arrived yet. Carried across
    /// [`Self::idle_poll`] calls so a line split by a poll-interval timeout
    /// (server writes it in two TCP segments straddling our short read
    /// deadline) is not silently dropped.
    idle_buf: Vec<u8>,
    /// Cached CAPABILITY. `uid_fetch_headers` consults this to decide
    /// whether `X-GM-THRID` is legal on FETCH — fail-closed: missing cache
    /// is fetched once, never guessed as Gmail.
    caps: Option<Capabilities>,
    /// Whether [`Self::ensure_condstore_enabled`] has already run for this
    /// session. `ENABLE CONDSTORE` is sent at most once, before the first
    /// `SELECT`; a server that never advertised CONDSTORE, or refused the
    /// `ENABLE`, is not asked again.
    condstore_attempted: bool,
}

impl ImapSession {
    pub fn connect(form: &MailboxForm, auth: ImapAuth) -> Result<Self, ConnectError> {
        let mut conn = wire::imap_connect(form)?;
        if conn.read_greeting {
            wire::expect_greeting(&mut conn.stream)?;
        }
        let mut session = Self {
            stream: conn.stream,
            tag_seq: 0,
            idle_tag: None,
            idle_buf: Vec::new(),
            caps: None,
            condstore_attempted: false,
        };
        match auth {
            ImapAuth::Login(password) => {
                let tag = session.next_tag();
                wire::imap_login(&mut session.stream, &tag, &form.email, &password)?;
            }
            ImapAuth::XOauth2(access_token) => {
                let tag = session.next_tag();
                wire::imap_authenticate_xoauth2(
                    &mut session.stream,
                    &tag,
                    &form.email,
                    &access_token,
                )?;
            }
        }
        Ok(session)
    }

    fn next_tag(&mut self) -> String {
        self.tag_seq += 1;
        format!("S{}", self.tag_seq)
    }

    pub fn capabilities(&mut self) -> Result<Capabilities, ConnectError> {
        if let Some(caps) = &self.caps {
            return Ok(caps.clone());
        }
        let tag = self.next_tag();
        let raw = wire::capability(&mut self.stream, &tag)?;
        let caps = Capabilities::from_raw(raw);
        self.caps = Some(caps.clone());
        Ok(caps)
    }

    pub fn list_folders(&mut self) -> Result<Vec<FolderListing>, ConnectError> {
        let tag = self.next_tag();
        wire::write_cmd(&mut self.stream, &tag, "LIST \"\" \"*\"")?;
        let lines = read_tagged_logical(&mut self.stream, &tag)?;
        let last = lines.last().cloned().unwrap_or_default();
        if !last.starts_with(&format!("{tag} OK")) {
            return Err(ConnectError::network(wire::sanitize(&last)));
        }
        let body = &lines[..lines.len().saturating_sub(1)];
        Ok(body.iter().filter_map(|l| parse_list_line(l)).collect())
    }

    /// APPEND one complete RFC822 message. The literal is written directly
    /// to the socket after the server continuation, without escaping or a
    /// second protocol-sized copy.
    pub fn append_message(
        &mut self,
        mailbox: &str,
        flags: &[&str],
        message: &[u8],
    ) -> Result<Option<u32>, ConnectError> {
        let tag = self.next_tag();
        let mailbox = wire::imap_quote(mailbox)?;
        let flags = if flags.is_empty() {
            String::new()
        } else {
            format!(" ({})", flags.join(" "))
        };
        write!(
            self.stream,
            "{tag} APPEND {mailbox}{flags} {{{}}}\r\n",
            message.len()
        )
        .map_err(|err| ConnectError::network(err.to_string()))?;
        self.stream
            .flush()
            .map_err(|err| ConnectError::network(err.to_string()))?;
        let continuation = wire::read_line(&mut self.stream)?;
        if !continuation.starts_with('+') {
            return Err(ConnectError::network(wire::sanitize(&continuation)));
        }
        self.stream
            .write_all(message)
            .and_then(|()| self.stream.write_all(b"\r\n"))
            .and_then(|()| self.stream.flush())
            .map_err(|err| ConnectError::network(err.to_string()))?;
        let lines = read_tagged_logical(&mut self.stream, &tag)?;
        let last = lines.last().cloned().unwrap_or_default();
        if last.starts_with(&format!("{tag} OK")) {
            Ok(append_uid(&last))
        } else {
            Err(ConnectError::network(wire::sanitize(&last)))
        }
    }

    /// Sends `ENABLE CONDSTORE` (RFC 7162 §3.1) once per session, in
    /// authenticated state before the first `SELECT` (RFC 5161 forbids
    /// `ENABLE` once a mailbox is selected).
    ///
    /// Until some CONDSTORE-enabling command has run, a server is not
    /// obliged to report `OK [HIGHESTMODSEQ n]` on `SELECT`, and without
    /// that number the sync engine can never start a `CHANGEDSINCE` delta
    /// (D30/D33): it has nothing to compare against, so it falls back to
    /// re-fetching every flag in the folder on every pass forever.
    ///
    /// Best-effort by design: a server that advertised CONDSTORE but
    /// answers `NO`/`BAD` here just keeps that periodic full flag resync,
    /// which is slower, not broken. The tagged response is still consumed
    /// either way, so the command stream stays in step.
    fn ensure_condstore_enabled(&mut self) -> Result<(), ConnectError> {
        if self.condstore_attempted {
            return Ok(());
        }
        if !self.capabilities()?.condstore {
            self.condstore_attempted = true;
            return Ok(());
        }
        self.condstore_attempted = true;
        let tag = self.next_tag();
        let _ = wire::tagged(&mut self.stream, &tag, "ENABLE CONDSTORE");
        Ok(())
    }

    pub fn select(&mut self, mailbox: &str) -> Result<SelectedMailbox, ConnectError> {
        // Before `next_tag`: both `capabilities()` and the `ENABLE` do
        // their own tagged round trip, and tags must stay in order.
        self.ensure_condstore_enabled()?;
        let tag = self.next_tag();
        let quoted = wire::imap_quote(mailbox)?;
        wire::write_cmd(&mut self.stream, &tag, &format!("SELECT {quoted}"))?;
        let lines = read_tagged_logical(&mut self.stream, &tag)?;
        let last = lines.last().cloned().unwrap_or_default();
        if !last.starts_with(&format!("{tag} OK")) {
            return Err(ConnectError::network(wire::sanitize(&last)));
        }
        let mut uidvalidity = None;
        let mut uidnext = None;
        let mut exists = None;
        let mut highest_modseq = None;
        for line in &lines {
            let upper = line.to_ascii_uppercase();
            if let Some(n) = number_before(&upper, " EXISTS") {
                exists = Some(n as u32);
            }
            if let Some(n) = number_after(&upper, "UIDVALIDITY") {
                uidvalidity = Some(n as u32);
            }
            if let Some(n) = number_after(&upper, "UIDNEXT") {
                uidnext = Some(n as u32);
            }
            if let Some(n) = number_after(&upper, "HIGHESTMODSEQ") {
                highest_modseq = Some(n);
            }
        }
        Ok(SelectedMailbox {
            uidvalidity: uidvalidity
                .ok_or_else(|| ConnectError::network("SELECT response missing UIDVALIDITY"))?,
            uidnext: uidnext
                .ok_or_else(|| ConnectError::network("SELECT response missing UIDNEXT"))?,
            exists: exists.unwrap_or(0),
            highest_modseq,
        })
    }

    /// `UID FETCH range (UID FLAGS INTERNALDATE RFC822.SIZE
    /// BODY.PEEK[HEADER.FIELDS (...)] [X-GM-THRID])`. Bodies are never
    /// fetched here (T-024). `X-GM-THRID` is included only when the
    /// cached CAPABILITY lists `X-GM-EXT-1` — a generic server answers
    /// BAD if that item is named.
    pub fn uid_fetch_headers(&mut self, range: UidRange) -> Result<Vec<HeaderMeta>, ConnectError> {
        let tag = self.next_tag();
        let cmd = format!(
            "UID FETCH {} ({})",
            range.to_wire(),
            self.header_fetch_items()?
        );
        self.run_fetch(&tag, &cmd)
    }

    fn header_fetch_items(&mut self) -> Result<String, ConnectError> {
        let mut items = String::from(
            "UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[HEADER.FIELDS (MESSAGE-ID IN-REPLY-TO REFERENCES FROM TO CC SUBJECT DATE)]",
        );
        if self.capabilities()?.x_gm_ext1 {
            items.push_str(" X-GM-THRID");
        }
        Ok(items)
    }

    /// `UID FETCH range (FLAGS) (CHANGEDSINCE modseq)` — CONDSTORE delta.
    pub fn uid_fetch_flags_changed_since(
        &mut self,
        range: UidRange,
        modseq: u64,
    ) -> Result<Vec<HeaderMeta>, ConnectError> {
        let tag = self.next_tag();
        let cmd = format!(
            "UID FETCH {} (FLAGS) (CHANGEDSINCE {modseq})",
            range.to_wire()
        );
        self.run_fetch(&tag, &cmd)
    }

    fn run_fetch(&mut self, tag: &str, cmd: &str) -> Result<Vec<HeaderMeta>, ConnectError> {
        wire::write_cmd(&mut self.stream, tag, cmd)?;
        let lines = read_tagged_logical(&mut self.stream, tag)?;
        let last = lines.last().cloned().unwrap_or_default();
        if !last.starts_with(&format!("{tag} OK")) {
            return Err(ConnectError::network(wire::sanitize(&last)));
        }
        let body = &lines[..lines.len().saturating_sub(1)];
        Ok(body.iter().filter_map(|l| parse_fetch_line(l)).collect())
    }

    /// `UID FETCH <uid> (BODY.PEEK[])` (T-024): the whole raw message,
    /// fetched on demand when the user opens it. Deliberately `BODY.PEEK[]`
    /// and never bare `BODY[]` -- the plain form sets `\Seen` as a
    /// server-side side effect (RFC 3501 §6.4.5), which would mark the
    /// message read behind our own `MarkRead` command's back the instant
    /// someone opens it. Reads the literal as exactly the `{n}` bytes the
    /// server declares, via `read_exact`, never by scanning for a
    /// terminator -- a body containing an embedded `\r\n` (or, in this
    /// fake-server test, even something that looks like a tagged response
    /// line) is not mistaken for the end of the literal.
    ///
    /// Returns the raw bytes as-is: this crate does not decode MIME/charset
    /// here, and never repeats any of the message's content in an error --
    /// failures below only ever say which UID and what kind of failure.
    pub fn uid_fetch_body_peek(&mut self, uid: u32) -> Result<Vec<u8>, ConnectError> {
        let tag = self.next_tag();
        wire::write_cmd(
            &mut self.stream,
            &tag,
            &format!("UID FETCH {uid} (BODY.PEEK[])"),
        )?;
        read_body_literal(&mut self.stream, &tag, uid)
    }

    /// Fetches several whole messages in one `UID FETCH` (T-024, batched).
    ///
    /// The warm-up asks for a hundred bodies at a time and each one used to
    /// be its own round trip: on the owner's mailbox that measured ~400 ms
    /// per body almost regardless of size, so a hundred bodies cost forty
    /// seconds of a connection that a click then had to queue behind. The
    /// bytes were never the problem; the trips were.
    ///
    /// `BODY.PEEK[]` for the same reason as the single fetch: reading a
    /// letter must not set `\Seen`, and neither must warming one.
    ///
    /// Bodies come back paired with the UID read out of each `FETCH`
    /// response, never by position: a server may answer in any order, may
    /// fold in untagged chatter, and may simply not return a UID that has
    /// since been expunged. Anything unparseable is skipped rather than
    /// mis-assigned -- attaching one message's bytes to another message's
    /// id would be a far worse failure than a missing warm body.
    pub fn uid_fetch_bodies_peek(
        &mut self,
        uids: &[u32],
    ) -> Result<Vec<(u32, Vec<u8>)>, ConnectError> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let set = uids
            .iter()
            .map(|uid| uid.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let tag = self.next_tag();
        wire::write_cmd(
            &mut self.stream,
            &tag,
            &format!("UID FETCH {set} (BODY.PEEK[])"),
        )?;
        read_body_literals(&mut self.stream, &tag, uids.len())
    }

    /// Fetches one MIME body section directly into `destination` (T-043).
    ///
    /// The section is transfer-decoded while the server literal is read; no
    /// complete message or attachment is retained in memory. `BODY.PEEK` is
    /// mandatory here just as it is for whole-message display: downloading an
    /// attachment must not silently set `\\Seen`.
    ///
    /// Invalid section paths are rejected before anything is written to the
    /// socket. They originate in our MIME metadata, but this validation keeps
    /// a malformed cached value from becoming an IMAP command injection.
    pub fn uid_fetch_section_to_file(
        &mut self,
        uid: u32,
        section: &str,
        transfer_encoding: TransferEncoding,
        destination: &Path,
        limits: AttachmentFetchLimits,
    ) -> Result<u64, ConnectError> {
        if !is_valid_body_section(section) {
            return Err(ConnectError::invalid("That attachment part isn't valid."));
        }
        let tag = self.next_tag();
        wire::write_cmd(
            &mut self.stream,
            &tag,
            &format!("UID FETCH {uid} (BODY.PEEK[{section}])"),
        )?;
        read_section_literal_to_file(
            &mut self.stream,
            &tag,
            uid,
            destination,
            transfer_encoding,
            limits,
        )
    }

    pub fn logout(&mut self) -> Result<(), ConnectError> {
        let tag = self.next_tag();
        // Best-effort: a server that drops the connection on LOGOUT before
        // sending the tagged OK should not surface as a user-facing error.
        let _ = wire::tagged(&mut self.stream, &tag, "LOGOUT");
        Ok(())
    }

    /// `CREATE "<name>"` (T-025). A server response reporting the mailbox
    /// already exists still comes back as `Err` here — [`crate::apply`]
    /// decides whether that is fine (D29: creating a folder that is already
    /// there is a no-op, not a failure), since only it knows the
    /// operation's retry semantics.
    ///
    /// `name` is the label the user typed, so it is encoded to modified
    /// UTF-7 (RFC 3501 §5.1.3) on the way out: a mailbox name is not an
    /// 8-bit string on the wire, and `UTF8=ACCEPT` (RFC 6855) is not
    /// negotiated here.
    pub fn create_mailbox(&mut self, name: &str) -> Result<(), ConnectError> {
        let quoted = wire::imap_quote(&crate::folders::encode_modified_utf7(name))?;
        self.run_ok(&format!("CREATE {quoted}"))
    }

    /// `RENAME "<from>" "<to>"` (T-060t). Both names are full mailbox paths
    /// in the server's own encoding, ready for the wire: this session knows
    /// the delimiter only for the mailboxes its own `LIST` returned, and
    /// the operation it is applying may have been queued long before that
    /// walk. A server `NO` (destination exists, source gone, hierarchy
    /// refused) comes back as `Err` -- unlike `CREATE`, there is no
    /// response here that means "already in the state you asked for".
    ///
    /// Neither name is encoded here (T-158). `from` is the path the server
    /// itself reported in `LIST`; `to` is assembled by [`crate::apply`]
    /// out of that same reported prefix plus the *one* newly encoded leaf.
    /// Encoding either one at this point would encode a prefix that is
    /// already encoded -- for a parent «Проекты» the `&` of `&BB8…` would
    /// be escaped again and the RENAME would name a mailbox that does not
    /// exist.
    pub fn rename_mailbox(&mut self, from: &str, to: &str) -> Result<(), ConnectError> {
        let from = wire::imap_quote(from)?;
        let to = wire::imap_quote(to)?;
        self.run_ok(&format!("RENAME {from} {to}"))
    }

    /// `DELETE "<name>"` (T-060u). The full mailbox path, computed by Core
    /// from `folders.remote_id` -- never rebuilt here, for the same reason
    /// [`Self::rename_mailbox`] does not rebuild one.
    ///
    /// RFC 3501 leaves deleting the *currently selected* mailbox
    /// implementation-defined, so [`crate::apply`] steps off the target
    /// before calling this. A `NO` for a mailbox that is already gone comes
    /// back as `Err`; only [`crate::apply`] knows that this particular
    /// operation treats "already gone" as done (D29).
    pub fn delete_mailbox(&mut self, name: &str) -> Result<(), ConnectError> {
        let quoted = wire::imap_quote(name)?;
        self.run_ok(&format!("DELETE {quoted}"))
    }

    /// `UID STORE <uids> (+|-)FLAGS (<flag>)` (T-025). Flags are idempotent
    /// on the wire (D29): re-adding a flag the server already has, or
    /// removing one it never had, is still `Ok`, never a conflict.
    pub fn uid_store_flag(
        &mut self,
        uids: &[u32],
        flag: &str,
        add: bool,
    ) -> Result<(), ConnectError> {
        if uids.is_empty() {
            return Ok(());
        }
        let op = if add { "+FLAGS" } else { "-FLAGS" };
        self.run_ok(&format!("UID STORE {} {op} ({flag})", uid_set(uids)))
    }

    /// `UID MOVE <uids> "<dest>"` (RFC 6851, T-025). Only meant to be sent
    /// when [`Capabilities::can_move`] is true; [`crate::apply`] falls back
    /// to [`Self::uid_copy`] + [`Self::uid_store_flag`] + [`Self::expunge`]
    /// on servers that never advertised MOVE.
    pub fn uid_move(&mut self, uids: &[u32], dest: &str) -> Result<(), ConnectError> {
        if uids.is_empty() {
            return Ok(());
        }
        let quoted = wire::imap_quote(dest)?;
        self.run_ok(&format!("UID MOVE {} {quoted}", uid_set(uids)))
    }

    /// `UID COPY <uids> "<dest>"` (T-025: step 1 of the no-MOVE fallback).
    pub fn uid_copy(&mut self, uids: &[u32], dest: &str) -> Result<(), ConnectError> {
        if uids.is_empty() {
            return Ok(());
        }
        let quoted = wire::imap_quote(dest)?;
        self.run_ok(&format!("UID COPY {} {quoted}", uid_set(uids)))
    }

    /// `EXPUNGE` (T-025: step 3 of the no-MOVE fallback — removes whatever
    /// is `\Deleted` in the currently selected mailbox).
    pub fn expunge(&mut self) -> Result<(), ConnectError> {
        self.run_ok("EXPUNGE")
    }

    /// `UID EXPUNGE <uids>` (RFC 4315 / UIDPLUS). Unlike plain `EXPUNGE`,
    /// this scopes permanent deletion to the UIDs this operation marked,
    /// so an unrelated message another client already marked `\Deleted`
    /// cannot be removed as collateral.
    pub fn uid_expunge(&mut self, uids: &[u32]) -> Result<(), ConnectError> {
        if uids.is_empty() {
            return Ok(());
        }
        self.run_ok(&format!("UID EXPUNGE {}", uid_set(uids)))
    }

    fn run_ok(&mut self, cmd: &str) -> Result<(), ConnectError> {
        let tag = self.next_tag();
        wire::write_cmd(&mut self.stream, &tag, cmd)?;
        let lines = read_tagged_logical(&mut self.stream, &tag)?;
        let last = lines.last().cloned().unwrap_or_default();
        if last.starts_with(&format!("{tag} OK")) {
            Ok(())
        } else {
            Err(ConnectError::network(wire::sanitize(&last)))
        }
    }

    // --- IDLE (T-026, RFC 2177). The deadline/DONE-timing policy lives in
    // `feathermail_providers::idle`; this impl only knows the raw wire
    // moves: send IDLE, wait (with a caller-chosen bound) for one untagged
    // line, send DONE. ---

    /// `TAG IDLE`, blocking for the server's `+` continuation (RFC 2177 §3)
    /// that means it is now sending unsolicited updates instead of
    /// expecting another command line.
    pub fn idle_start(&mut self) -> Result<(), ConnectError> {
        let tag = self.next_tag();
        wire::write_cmd(&mut self.stream, &tag, "IDLE")?;
        let line = wire::read_line(&mut self.stream)?;
        if !line.trim_start().starts_with('+') {
            return Err(ConnectError::network(wire::sanitize(&line)));
        }
        self.idle_tag = Some(tag);
        Ok(())
    }

    /// One bounded wait slice while IDLE is active. `Ok(None)` means
    /// `poll_interval` elapsed with nothing on the wire — an expected,
    /// ordinary outcome the caller (`feathermail_providers::idle`) uses to
    /// re-check its own ~29-minute ceiling and any stop signal, never
    /// itself an error. `Ok(Some(event))` is the first complete untagged
    /// line. `Err` is a real transport failure (anything other than the
    /// read simply timing out).
    pub fn idle_poll(
        &mut self,
        poll_interval: Duration,
    ) -> Result<Option<IdleEvent>, ConnectError> {
        self.stream.set_read_timeout(Some(poll_interval))?;
        let mut byte = [0u8; 1];
        loop {
            match self.stream.read(&mut byte) {
                Ok(0) => return Err(ConnectError::network("connection closed during IDLE")),
                Ok(_) => {
                    if byte[0] == b'\n' {
                        let line = String::from_utf8_lossy(&self.idle_buf)
                            .trim_end_matches('\r')
                            .to_string();
                        self.idle_buf.clear();
                        return Ok(Some(parse_idle_line(&line)));
                    }
                    if byte[0] != b'\r' {
                        self.idle_buf.push(byte[0]);
                        if self.idle_buf.len() > 16_384 {
                            self.idle_buf.clear();
                            return Err(ConnectError::network("IMAP line too long"));
                        }
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    return Ok(None);
                }
                Err(e) => return Err(wire::net(e)),
            }
        }
    }

    /// `DONE` — end IDLE and wait for the tagged `OK` that closes it out.
    /// Restores the normal blocking read timeout so the session behaves
    /// like every other command afterward (a caller forgetting this after
    /// `idle_poll` shortened it would see spurious timeouts on the very
    /// next ordinary command).
    pub fn idle_done(&mut self) -> Result<(), ConnectError> {
        let tag = self
            .idle_tag
            .take()
            .ok_or_else(|| ConnectError::invalid("not currently in IDLE"))?;
        self.idle_buf.clear();
        self.stream.set_read_timeout(Some(wire::TIMEOUT))?;
        self.stream.write_all(b"DONE\r\n").map_err(wire::net)?;
        self.stream.flush().map_err(wire::net)?;
        let lines = read_tagged_logical(&mut self.stream, &tag)?;
        let last = lines.last().cloned().unwrap_or_default();
        if last.starts_with(&format!("{tag} OK")) {
            Ok(())
        } else {
            Err(ConnectError::network(wire::sanitize(&last)))
        }
    }

    /// `NOOP`, used as the poll primitive when the server has no IDLE
    /// (D30: "иначе poll" is an explicit, honest fallback, not silent
    /// inaction) — any mailbox changes the server wants to report can ride
    /// along as untagged responses on an otherwise no-op command.
    pub fn noop_check(&mut self) -> Result<Vec<IdleEvent>, ConnectError> {
        let tag = self.next_tag();
        wire::write_cmd(&mut self.stream, &tag, "NOOP")?;
        let lines = read_tagged_logical(&mut self.stream, &tag)?;
        let last = lines.last().cloned().unwrap_or_default();
        if !last.starts_with(&format!("{tag} OK")) {
            return Err(ConnectError::network(wire::sanitize(&last)));
        }
        let body = &lines[..lines.len().saturating_sub(1)];
        Ok(body.iter().map(|l| parse_idle_line(l)).collect())
    }
}

/// Extracts the assigned UID from an optional UIDPLUS `APPENDUID` response.
/// The UIDVALIDITY token is intentionally ignored here: T-042 stores only
/// the opaque UID as a latest-server locator and never uses it to delete a
/// draft, so accepting a UIDVALIDITY change cannot target another message.
fn append_uid(tagged: &str) -> Option<u32> {
    let marker = "[APPENDUID ";
    let rest = tagged.get(tagged.find(marker)? + marker.len()..)?;
    let mut fields = rest.split_whitespace();
    fields.next()?.parse::<u32>().ok()?;
    fields.next()?.trim_end_matches(']').parse().ok()
}

/// `1,2,3` — a plain comma-joined UID set. No range compression: the
/// operations this module issues only ever act on the handful of UIDs one
/// thread maps to, so a compact `n:m` range never actually applies, and a
/// flat list keeps the fake-server-facing wire format trivial to parse.
fn uid_set(uids: &[u32]) -> String {
    uids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// One untagged response seen while IDLE (or the NOOP poll fallback) is
/// active (RFC 2177 §3 / T-026): new/removed/changed messages, which is
/// exactly what sync (T-022) needs to know to decide whether it's worth
/// resyncing this folder right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdleEvent {
    /// `* n EXISTS` — the mailbox now has `n` messages (new mail, if larger
    /// than the count from the last `SELECT`).
    Exists(u32),
    /// `* n EXPUNGE` — message sequence number `n` was removed.
    Expunge(u32),
    /// `* n FETCH ...` — flags (or other attributes) changed on message `n`.
    Fetch(u32),
    /// Anything else untagged (e.g. `* OK ...`) — still evidence that the
    /// server said *something* unsolicited, forwarded rather than silently
    /// dropped so a caller can decide whether it's worth a resync.
    Other(String),
}

/// Classify one untagged line seen during IDLE/NOOP. Bounds-checked like
/// the rest of this module's parsing: an unrecognized line just becomes
/// [`IdleEvent::Other`], never a panic.
fn parse_idle_line(line: &str) -> IdleEvent {
    let upper = line.to_ascii_uppercase();
    if let Some(n) = number_before(&upper, " EXISTS") {
        return IdleEvent::Exists(n as u32);
    }
    if let Some(n) = number_before(&upper, " EXPUNGE") {
        return IdleEvent::Expunge(n as u32);
    }
    if let Some(n) = number_before(&upper, " FETCH") {
        return IdleEvent::Fetch(n as u32);
    }
    IdleEvent::Other(wire::sanitize(line))
}

// --- Response reading: literal-aware so header text / non-ASCII names never
// get split on an embedded CRLF. ---

/// One logical IMAP response line. If the raw line ends in a literal marker
/// (`{n}`), the next `n` bytes are read verbatim and re-inserted as an
/// escaped quoted string so the rest of this module can use one plain
/// tokenizer for both literals and real quoted strings.
fn read_logical_line<S: Read>(stream: &mut S) -> Result<String, ConnectError> {
    let mut text = String::new();
    loop {
        let line = wire::read_line(stream)?;
        match trailing_literal_len(&line) {
            Some(n) => {
                if n > MAX_LITERAL_BYTES {
                    return Err(ConnectError::network("IMAP literal too large"));
                }
                let cut = line
                    .rfind('{')
                    .ok_or_else(|| ConnectError::network("IMAP malformed literal marker"))?;
                text.push_str(&line[..cut]);
                let mut lit = vec![0u8; n];
                stream.read_exact(&mut lit).map_err(wire::net)?;
                let lit_str = String::from_utf8_lossy(&lit);
                text.push('"');
                for ch in lit_str.chars() {
                    if ch == '"' || ch == '\\' {
                        text.push('\\');
                    }
                    text.push(ch);
                }
                text.push('"');
                // The stream continues right after the literal bytes with the
                // rest of this same logical response; keep reading.
            }
            None => {
                text.push_str(&line);
                return Ok(text);
            }
        }
    }
}

fn read_tagged_logical<S: Read>(stream: &mut S, tag: &str) -> Result<Vec<String>, ConnectError> {
    let mut lines = Vec::new();
    let prefix = format!("{tag} ");
    loop {
        let line = read_logical_line(stream)?;
        let done = line.starts_with(&prefix);
        lines.push(line);
        if done {
            return Ok(lines);
        }
        if lines.len() > MAX_RESPONSE_LINES {
            return Err(ConnectError::network("IMAP response too long"));
        }
    }
}

/// Reads the `UID FETCH ... (BODY.PEEK[])` response for `tag`: scans lines
/// (untagged chatter is skipped) until one ends in a `{n}` literal marker,
/// then reads exactly `n` raw bytes -- never through
/// [`read_logical_line`]'s `String::from_utf8_lossy` re-encoding, which is
/// fine for short, mostly-ASCII header blobs but would silently mangle a
/// body that isn't valid UTF-8 (a Latin-1 HTML part, a raw attachment,
/// ...). If the tagged completion line arrives before any literal was ever
/// seen (nothing to fetch, e.g. a UID that no longer exists), that is
/// treated as an empty body rather than an error -- distinguishing "no
/// body was fetched" from "the fetch failed" is [`crate::body`]'s job on
/// the Core side, not this wire layer's.
fn read_body_literal<S: Read>(
    stream: &mut S,
    tag: &str,
    uid: u32,
) -> Result<Vec<u8>, ConnectError> {
    let prefix = format!("{tag} ");
    let mut seen_lines = 0usize;
    loop {
        seen_lines += 1;
        if seen_lines > MAX_RESPONSE_LINES {
            return Err(ConnectError::network(format!(
                "UID {uid}: IMAP response too long"
            )));
        }
        let line = wire::read_line(stream)?;
        if let Some(n) = trailing_literal_len(&line) {
            if n > MAX_BODY_LITERAL_BYTES {
                return Err(ConnectError::network(format!(
                    "UID {uid}: IMAP body literal too large"
                )));
            }
            let mut body = vec![0u8; n];
            stream.read_exact(&mut body).map_err(wire::net)?;
            drain_after_literal(stream, &prefix, uid)?;
            return Ok(body);
        }
        if line.starts_with(&prefix) {
            return if line.starts_with(&format!("{tag} OK")) {
                Ok(Vec::new())
            } else {
                Err(ConnectError::network(format!(
                    "UID {uid}: server rejected BODY.PEEK fetch"
                )))
            };
        }
        // Untagged chatter unrelated to this fetch (e.g. a concurrent
        // `* n EXISTS`) -- keep reading.
    }
}

/// Reads one MIME section literal into an attachment cache file. Unlike
/// [`read_body_literal`], this never allocates based on the server's literal
/// length. A failed decode is followed by a fixed-buffer drain of the rest of
/// that literal and its tagged completion so callers may safely reuse the
/// session after a recoverable cache/size error.
fn read_section_literal_to_file<S: Read>(
    stream: &mut S,
    tag: &str,
    uid: u32,
    destination: &Path,
    transfer_encoding: TransferEncoding,
    limits: AttachmentFetchLimits,
) -> Result<u64, ConnectError> {
    let prefix = format!("{tag} ");
    let mut seen_lines = 0usize;
    loop {
        seen_lines += 1;
        if seen_lines > MAX_RESPONSE_LINES {
            return Err(ConnectError::network(format!(
                "UID {uid}: IMAP response too long"
            )));
        }
        let line = wire::read_line(stream)?;
        if let Some(literal_len) = trailing_literal_len(&line) {
            let literal_len = u64::try_from(literal_len).map_err(|_| {
                ConnectError::network(format!("UID {uid}: attachment literal is invalid"))
            })?;
            let mut literal = stream.take(literal_len);

            if literal_len > limits.max_wire_bytes {
                drain_literal(&mut literal, uid)?;
                drain_after_literal(stream, &prefix, uid)?;
                return Err(ConnectError::network(format!(
                    "UID {uid}: attachment literal exceeds configured limit"
                )));
            }

            // T-112: `decode_to_file` writes to a `.part` file and renames
            // it into place only after the decode returns `Ok`, so the one
            // thing that must never look like `Ok` here is the server
            // hanging up mid-literal. A bare `Take` reports that as a plain
            // EOF, the decoder calls the stream finished, and a half
            // attachment gets renamed over the cache path -- where the next
            // download attempt finds `destination.is_file()` and marks the
            // truncated file as a good cache entry. `ExactLiteral` turns the
            // early EOF into an `io::Error` instead, so the `.part` file is
            // removed and the cache path stays untouched.
            let decoded = decode_to_file(
                ExactLiteral {
                    literal: &mut literal,
                },
                destination,
                transfer_encoding,
                Some(limits.max_decoded_bytes),
            );
            // `decode_to_file` stops as soon as writing/decoding fails. The
            // remaining literal must still be consumed before reading the
            // FETCH response's closing line.
            drain_literal(&mut literal, uid)?;
            drain_after_literal(stream, &prefix, uid)?;

            return decoded.map_err(|_| {
                ConnectError::network(format!("UID {uid}: attachment download failed"))
            });
        }
        if line.starts_with(&prefix) {
            return if line.starts_with(&format!("{tag} OK")) {
                Err(ConnectError::network(format!(
                    "UID {uid}: attachment part was not returned"
                )))
            } else {
                Err(ConnectError::network(format!(
                    "UID {uid}: server rejected attachment fetch"
                )))
            };
        }
    }
}

/// The declared literal, and nothing less (T-112). `Take` alone cannot tell
/// "the literal ended" from "the connection died half way through it": both
/// surface as `read` returning `0`. Every consumer of a literal in this file
/// needs the difference, but this one needs it *before* the bytes are
/// committed anywhere -- see the call site in
/// [`read_section_literal_to_file`].
struct ExactLiteral<'a, S: Read> {
    literal: &'a mut std::io::Take<S>,
}

impl<S: Read> Read for ExactLiteral<'_, S> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.literal.read(buf)?;
        if read == 0 && self.literal.limit() > 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "IMAP literal ended early",
            ));
        }
        Ok(read)
    }
}

/// Consume the un-read tail of a declared literal with a fixed stack buffer.
/// Returning an error for EOF is important: a short literal must never be
/// mistaken for a successfully downloaded attachment.
fn drain_literal<S: Read>(
    literal: &mut std::io::Take<&mut S>,
    uid: u32,
) -> Result<(), ConnectError> {
    let mut buffer = [0_u8; feathermail_attachments::STREAM_CHUNK];
    while literal.limit() > 0 {
        let read = literal.read(&mut buffer).map_err(wire::net)?;
        if read == 0 {
            return Err(ConnectError::network(format!(
                "UID {uid}: attachment literal was truncated"
            )));
        }
    }
    Ok(())
}

/// IMAP body section paths are decimal MIME part numbers (`1`, `2.1`, ...),
/// plus `TEXT` for a top-level, non-multipart attachment. Keep the grammar
/// intentionally narrow: section specifiers such as `HEADER.FIELDS` are not
/// attachment payloads, and accepting arbitrary text here would let
/// corrupted local metadata alter a command line.
fn is_valid_body_section(section: &str) -> bool {
    section == "TEXT"
        || (!section.is_empty()
            && section.split('.').all(|part| {
                !part.is_empty()
                    && !part.starts_with('0')
                    && part.bytes().all(|byte| byte.is_ascii_digit())
            }))
}

/// Drains whatever the server sends right after the body literal's raw
/// bytes -- normally just the closing `)` and the tagged completion line --
/// until the tagged line shows up.
/// Reads the literals of a multi-message `UID FETCH` until the tagged
/// completion, pairing each with the UID named in its own `FETCH` line.
///
/// `expected` bounds only the reply: a server that answers with more
/// `FETCH` responses than were asked for is answering about messages the
/// caller never named, and the extra ones are dropped rather than stored.
fn read_body_literals<S: Read>(
    stream: &mut S,
    tag: &str,
    expected: usize,
) -> Result<Vec<(u32, Vec<u8>)>, ConnectError> {
    let prefix = format!("{tag} ");
    let mut out: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut kept = 0usize;
    let mut seen_lines = 0usize;
    loop {
        seen_lines += 1;
        if seen_lines > MAX_RESPONSE_LINES {
            return Err(ConnectError::network(
                "batched UID FETCH: IMAP response too long".to_string(),
            ));
        }
        let line = wire::read_line(stream)?;
        if let Some(n) = trailing_literal_len(&line) {
            if n > MAX_BODY_LITERAL_BYTES {
                return Err(ConnectError::network(
                    "batched UID FETCH: IMAP body literal too large".to_string(),
                ));
            }
            let mut body = vec![0u8; n];
            stream.read_exact(&mut body).map_err(wire::net)?;
            // The bytes have to be consumed whichever way this goes -- the
            // stream is shared with everything that follows on this
            // session, so a literal we cannot keep is still read off it.
            let room = kept + n <= MAX_BATCH_BODY_BYTES;
            match uid_in_fetch_line(&line) {
                Some(uid) if room && out.len() < expected => {
                    kept += n;
                    out.push((uid, body));
                }
                _ => {}
            }
            continue;
        }
        if line.starts_with(&prefix) {
            return if line.starts_with(&format!("{tag} OK")) {
                Ok(out)
            } else {
                Err(ConnectError::network(
                    "batched UID FETCH: server rejected BODY.PEEK fetch".to_string(),
                ))
            };
        }
        // Untagged chatter between responses (`* n EXISTS`, the lone `)`
        // that closes a FETCH) -- keep reading.
    }
}

/// The UID out of a `* 12 FETCH (UID 101 BODY[] {1234}` line.
///
/// Case-insensitive on the keyword and tolerant of attribute order, since
/// neither is fixed by the grammar. Returns `None` rather than guessing.
fn uid_in_fetch_line(line: &str) -> Option<u32> {
    let upper = line.to_ascii_uppercase();
    let mut from = 0usize;
    while let Some(found) = upper[from..].find("UID ") {
        let at = from + found;
        // Must be a whole token: `UID` after `(` or whitespace, so that a
        // subject containing the word cannot be read as the attribute.
        let boundary_ok = at == 0 || matches!(upper.as_bytes()[at - 1], b' ' | b'(' | b'\t');
        let digits: String = upper[at + 4..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if boundary_ok && !digits.is_empty() {
            return digits.parse().ok();
        }
        from = at + 4;
    }
    None
}

fn drain_after_literal<S: Read>(
    stream: &mut S,
    prefix: &str,
    uid: u32,
) -> Result<(), ConnectError> {
    let mut seen_lines = 0usize;
    loop {
        seen_lines += 1;
        if seen_lines > MAX_RESPONSE_LINES {
            return Err(ConnectError::network(format!(
                "UID {uid}: IMAP response too long"
            )));
        }
        let line = wire::read_line(stream)?;
        if line.starts_with(prefix) {
            return if line.starts_with(&format!("{prefix}OK")) {
                Ok(())
            } else {
                Err(ConnectError::network(format!(
                    "UID {uid}: server rejected BODY.PEEK fetch"
                )))
            };
        }
    }
}

fn trailing_literal_len(line: &str) -> Option<usize> {
    let trimmed = line.trim_end();
    if !trimmed.ends_with('}') {
        return None;
    }
    let start = trimmed.rfind('{')?;
    let digits = trimmed[start + 1..trimmed.len() - 1].trim_end_matches('+');
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<usize>().ok()
}

fn number_before(line: &str, suffix: &str) -> Option<u64> {
    let pos = line.find(suffix)?;
    let before = &line[..pos];
    let digits: String = before
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits.chars().rev().collect::<String>().parse().ok()
}

fn number_after(line: &str, key: &str) -> Option<u64> {
    let pos = line.find(key)?;
    let rest = line[pos + key.len()..].trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

// --- Minimal IMAP value tokenizer (atoms / quoted strings / parenthesized
// lists). Literals have already been rewritten to quoted strings by
// `read_logical_line`, so this never has to see one directly. Bounds-checked
// throughout; a malformed line just yields fewer/odd tokens, never a panic. ---

#[derive(Debug, Clone)]
enum Value {
    Atom(String),
    Str(String),
    List(Vec<Value>),
}

fn tokenize_line(s: &str) -> Vec<Value> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    parse_seq(&chars, &mut i, None)
}

fn parse_seq(chars: &[char], i: &mut usize, stop: Option<char>) -> Vec<Value> {
    let mut out = Vec::new();
    while *i < chars.len() {
        let c = chars[*i];
        if let Some(s) = stop {
            if c == s {
                *i += 1;
                break;
            }
        }
        if c.is_whitespace() {
            *i += 1;
            continue;
        }
        if c == '(' {
            *i += 1;
            out.push(Value::List(parse_seq(chars, i, Some(')'))));
            continue;
        }
        if c == '"' {
            *i += 1;
            let mut buf = String::new();
            while *i < chars.len() {
                let c2 = chars[*i];
                if c2 == '\\' && *i + 1 < chars.len() {
                    buf.push(chars[*i + 1]);
                    *i += 2;
                    continue;
                }
                if c2 == '"' {
                    *i += 1;
                    break;
                }
                buf.push(c2);
                *i += 1;
            }
            out.push(Value::Str(buf));
            continue;
        }
        // Atom, allowing a bracketed section (BODY[HEADER.FIELDS (...)])
        // that may itself contain spaces and parens.
        let start = *i;
        let mut depth = 0i32;
        while *i < chars.len() {
            let c2 = chars[*i];
            if depth == 0 && (c2.is_whitespace() || c2 == '(' || c2 == ')') {
                break;
            }
            if c2 == '[' {
                depth += 1;
            }
            if c2 == ']' {
                depth -= 1;
            }
            *i += 1;
        }
        if *i == start {
            // Stray unmatched char (e.g. lone ')'); skip it so we always
            // make progress instead of looping forever on garbage input.
            *i += 1;
            continue;
        }
        out.push(Value::Atom(chars[start..*i].iter().collect()));
    }
    out
}

fn parse_list_line(line: &str) -> Option<FolderListing> {
    let values = tokenize_line(line);
    if values.len() < 5 {
        return None;
    }
    if !matches!(&values[1], Value::Atom(a) if a.eq_ignore_ascii_case("LIST")) {
        return None;
    }
    let attrs = match &values[2] {
        Value::List(items) => items,
        _ => return None,
    };
    let attr_strs: Vec<String> = attrs
        .iter()
        .filter_map(|v| match v {
            Value::Atom(a) => Some(a.clone()),
            _ => None,
        })
        .collect();
    let has_children = attr_strs
        .iter()
        .any(|a| a.eq_ignore_ascii_case("\\HasChildren"));
    let no_select = attr_strs
        .iter()
        .any(|a| a.eq_ignore_ascii_case("\\Noselect"));
    let delimiter = match &values[3] {
        Value::Str(s) => s.chars().next(),
        Value::Atom(a) if !a.eq_ignore_ascii_case("NIL") => a.chars().next(),
        _ => None,
    };
    let name = match &values[4] {
        Value::Str(s) => s.clone(),
        Value::Atom(a) => a.clone(),
        _ => return None,
    };
    let kind = folder_kind_from(&attr_strs, &name);
    Some(FolderListing {
        name,
        delimiter,
        has_children,
        no_select,
        kind,
    })
}

/// SPECIAL-USE (RFC 6154) mapping. `\Inbox` is rare in the wild (most
/// servers just name the mailbox `INBOX`), so that name is checked too.
fn folder_kind_from(attrs: &[String], name: &str) -> FolderKind {
    for a in attrs {
        match a.to_ascii_lowercase().as_str() {
            "\\inbox" => return FolderKind::Inbox,
            "\\sent" => return FolderKind::Sent,
            "\\drafts" => return FolderKind::Drafts,
            "\\trash" => return FolderKind::Trash,
            "\\junk" => return FolderKind::Spam,
            "\\archive" => return FolderKind::Archive,
            _ => {}
        }
    }
    if name.eq_ignore_ascii_case("INBOX") {
        return FolderKind::Inbox;
    }
    FolderKind::Custom
}

fn parse_fetch_line(line: &str) -> Option<HeaderMeta> {
    let values = tokenize_line(line);
    let data = values.iter().find_map(|v| match v {
        Value::List(items) => Some(items),
        _ => None,
    })?;

    let mut uid = None;
    let mut flags = Vec::new();
    let mut internaldate = None;
    let mut size_bytes = None;
    let mut header_text: Option<String> = None;
    let mut gm_thrid = None;

    let mut i = 0;
    while i + 1 < data.len() {
        let key = match &data[i] {
            Value::Atom(a) => a.to_ascii_uppercase(),
            _ => {
                i += 1;
                continue;
            }
        };
        let value = &data[i + 1];
        if key == "UID" {
            if let Value::Atom(n) = value {
                uid = n.parse::<u32>().ok();
            }
        } else if key == "FLAGS" {
            if let Value::List(items) = value {
                flags = items
                    .iter()
                    .filter_map(|v| match v {
                        Value::Atom(a) => Some(a.clone()),
                        _ => None,
                    })
                    .collect();
            }
        } else if key == "INTERNALDATE" {
            if let Value::Str(s) = value {
                internaldate = Some(s.clone());
            }
        } else if key == "RFC822.SIZE" {
            if let Value::Atom(n) = value {
                size_bytes = n.parse::<u64>().ok();
            }
        } else if key == "X-GM-THRID" {
            if let Value::Atom(n) = value {
                gm_thrid = Some(n.clone());
            }
        } else if key.starts_with("BODY[") || key.starts_with("BODY.PEEK[") {
            if let Value::Str(s) = value {
                header_text = Some(s.clone());
            }
        }
        i += 2;
    }

    let uid = uid?;
    let fields = header_text
        .as_deref()
        .map(parse_header_fields)
        .unwrap_or_default();
    Some(HeaderMeta {
        uid,
        flags,
        internaldate,
        size_bytes,
        message_id: fields.message_id,
        in_reply_to: fields.in_reply_to,
        references: fields.references,
        from: fields.from,
        to: fields.to,
        cc: fields.cc,
        subject: fields.subject,
        date: fields.date,
        gm_thrid,
    })
}

#[derive(Default)]
struct HeaderFields {
    message_id: Option<String>,
    in_reply_to: Option<String>,
    references: Vec<String>,
    from: Option<String>,
    to: Option<String>,
    cc: Option<String>,
    subject: Option<String>,
    date: Option<String>,
}

/// Minimal RFC 5322 header-fields parser (unfolds continuation lines). Only
/// the fields we requested in `BODY.PEEK[HEADER.FIELDS (...)]` are recognized.
fn parse_header_fields(raw: &str) -> HeaderFields {
    let mut out = HeaderFields::default();
    let mut current_key: Option<String> = None;
    let mut current_val = String::new();
    let normalized = raw.replace("\r\n", "\n");
    for line in normalized.split('\n') {
        if line.is_empty() {
            continue;
        }
        if (line.starts_with(' ') || line.starts_with('\t')) && current_key.is_some() {
            current_val.push(' ');
            current_val.push_str(line.trim());
            continue;
        }
        if let Some(key) = current_key.take() {
            apply_header_field(&mut out, &key, current_val.trim());
        }
        current_val.clear();
        if let Some((k, v)) = line.split_once(':') {
            current_key = Some(k.trim().to_ascii_lowercase());
            current_val = v.trim().to_string();
        }
    }
    if let Some(key) = current_key.take() {
        apply_header_field(&mut out, &key, current_val.trim());
    }
    out
}

fn apply_header_field(out: &mut HeaderFields, key: &str, val: &str) {
    if val.is_empty() {
        return;
    }
    match key {
        "message-id" => out.message_id = Some(val.to_string()),
        "in-reply-to" => out.in_reply_to = Some(val.to_string()),
        "references" => out.references = val.split_whitespace().map(str::to_string).collect(),
        "from" => out.from = Some(val.to_string()),
        "to" => out.to = Some(val.to_string()),
        "cc" => out.cc = Some(val.to_string()),
        "subject" => out.subject = Some(val.to_string()),
        "date" => out.date = Some(val.to_string()),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use feathermail_core::MailSecurity;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;

    fn form(port: u16) -> MailboxForm {
        MailboxForm {
            email: "you@example.com".into(),
            imap_host: "127.0.0.1".into(),
            imap_port: port,
            imap_security: MailSecurity::None,
            smtp_host: "127.0.0.1".into(),
            smtp_port: 0,
            smtp_security: MailSecurity::None,
        }
    }

    /// Fake Dovecot-ish server: LOGIN, CAPABILITY, LIST (special-use), SELECT
    /// (UIDVALIDITY/UIDNEXT/HIGHESTMODSEQ), UID FETCH with a literal header
    /// blob, LOGOUT.
    fn spawn_full_server() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            write!(writer, "* OK fake ready\r\n").unwrap();
            writer.flush().unwrap();
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let upper = line.to_ascii_uppercase();
                let tag = line.split_whitespace().next().unwrap_or("*").to_string();
                if upper.contains(" LOGIN ") {
                    write!(writer, "{tag} OK logged in\r\n").unwrap();
                } else if upper.contains(" CAPABILITY") {
                    write!(
                        writer,
                        "* CAPABILITY IMAP4rev1 CONDSTORE UIDPLUS IDLE MOVE\r\n{tag} OK CAPABILITY completed\r\n"
                    )
                    .unwrap();
                } else if upper.contains(" LIST ") {
                    write!(writer, "* LIST (\\HasNoChildren) \"/\" INBOX\r\n").unwrap();
                    write!(
                        writer,
                        "* LIST (\\HasNoChildren \\Sent) \"/\" \"Sent Items\"\r\n"
                    )
                    .unwrap();
                    write!(
                        writer,
                        "* LIST (\\Noselect \\HasChildren) \"/\" \"[Gmail]\"\r\n"
                    )
                    .unwrap();
                    write!(writer, "{tag} OK LIST completed\r\n").unwrap();
                } else if upper.contains(" SELECT ") {
                    write!(writer, "* FLAGS (\\Answered \\Seen)\r\n").unwrap();
                    write!(writer, "* 3 EXISTS\r\n").unwrap();
                    write!(writer, "* 0 RECENT\r\n").unwrap();
                    write!(writer, "* OK [UIDVALIDITY 123456] UIDs valid\r\n").unwrap();
                    write!(writer, "* OK [UIDNEXT 42] Predicted next UID\r\n").unwrap();
                    write!(writer, "* OK [HIGHESTMODSEQ 999] highest\r\n").unwrap();
                    write!(writer, "{tag} OK [READ-WRITE] SELECT completed\r\n").unwrap();
                } else if upper.contains("UID FETCH") {
                    let header = "Subject: Hi there\r\nFrom: a@b.com\r\nCc: copy@example.com\r\nMessage-ID: <abc@x>\r\n";
                    write!(
                        writer,
                        "* 1 FETCH (UID 7 FLAGS (\\Seen) INTERNALDATE \"01-Jan-2024 00:00:00 +0000\" RFC822.SIZE 1234 BODY[HEADER.FIELDS (MESSAGE-ID IN-REPLY-TO REFERENCES FROM TO CC SUBJECT DATE)] {{{}}}\r\n",
                        header.len()
                    )
                    .unwrap();
                    write!(writer, "{header}").unwrap();
                    write!(writer, ")\r\n").unwrap();
                    write!(writer, "{tag} OK UID FETCH completed\r\n").unwrap();
                } else if upper.contains("LOGOUT") {
                    write!(writer, "* BYE\r\n{tag} OK LOGOUT\r\n").unwrap();
                    break;
                } else {
                    write!(writer, "{tag} BAD unknown\r\n").unwrap();
                }
                writer.flush().unwrap();
            }
        });
        port
    }

    /// A fake IMAP server for the mailbox-name and CONDSTORE tests.
    ///
    /// Records every command line it is sent, and -- like a real CONDSTORE
    /// server (RFC 7162 3.1.2) -- reports `HIGHESTMODSEQ` on `SELECT` only
    /// once a CONDSTORE-enabling command has run in this session
    /// (`ENABLE CONDSTORE`, or `SELECT ... (CONDSTORE)`).
    fn spawn_recording_server(
        advertise_condstore: bool,
    ) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let log2 = log.clone();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            write!(writer, "* OK fake ready\r\n").unwrap();
            writer.flush().unwrap();
            let mut condstore_enabled = false;
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let trimmed = line.trim_end().to_string();
                log2.lock().unwrap().push(trimmed.clone());
                let upper = trimmed.to_ascii_uppercase();
                let tag = trimmed.split_whitespace().next().unwrap_or("*").to_string();
                if upper.contains(" LOGIN ") {
                    write!(writer, "{tag} OK logged in\r\n").unwrap();
                } else if upper.contains(" CAPABILITY") {
                    let condstore = if advertise_condstore {
                        "CONDSTORE "
                    } else {
                        ""
                    };
                    write!(
                        writer,
                        "* CAPABILITY IMAP4rev1 {condstore}UIDPLUS IDLE MOVE\r\n{tag} OK CAPABILITY completed\r\n"
                    )
                    .unwrap();
                } else if upper.contains(" ENABLE ") {
                    if upper.contains("CONDSTORE") {
                        condstore_enabled = true;
                        write!(writer, "* ENABLED CONDSTORE\r\n").unwrap();
                    }
                    write!(writer, "{tag} OK ENABLE completed\r\n").unwrap();
                } else if upper.contains(" SELECT ") {
                    if upper.contains("(CONDSTORE)") {
                        condstore_enabled = true;
                    }
                    write!(writer, "* 3 EXISTS\r\n").unwrap();
                    write!(writer, "* OK [UIDVALIDITY 123456] UIDs valid\r\n").unwrap();
                    write!(writer, "* OK [UIDNEXT 42] Predicted next UID\r\n").unwrap();
                    if condstore_enabled {
                        write!(writer, "* OK [HIGHESTMODSEQ 999] highest\r\n").unwrap();
                    } else {
                        write!(writer, "* OK [NOMODSEQ] no modseqs yet\r\n").unwrap();
                    }
                    write!(writer, "{tag} OK [READ-WRITE] SELECT completed\r\n").unwrap();
                } else {
                    write!(writer, "{tag} OK completed\r\n").unwrap();
                }
                writer.flush().unwrap();
            }
        });
        (port, log)
    }

    /// `CREATE` still encodes the label it is handed (it is a bare label,
    /// typed by the user). `RENAME` no longer encodes anything: T-158 moved
    /// that one step up, to [`crate::apply`], which encodes the leaf alone
    /// and leaves the `LIST`-reported prefix as it is. So a destination
    /// whose parent is *already* encoded must reach the wire byte for byte.
    #[test]
    fn create_encodes_its_label_and_rename_sends_both_paths_verbatim() {
        let (port, log) = spawn_recording_server(true);
        thread::sleep(Duration::from_millis(30));
        let mut session = ImapSession::connect(&form(port), ImapAuth::Login("x".into())).unwrap();
        session
            .create_mailbox("\u{41f}\u{440}\u{43e}\u{435}\u{43a}\u{442}\u{44b}")
            .unwrap();
        // «Проекты/Ideas» -> «Проекты/Идеи», as `apply` assembles it.
        session
            .rename_mailbox(
                "&BB8EQAQ+BDUEOgRCBEs-/Ideas",
                "&BB8EQAQ+BDUEOgRCBEs-/&BBgENAQ1BDg-",
            )
            .unwrap();

        let lines = log.lock().unwrap().clone();
        let create = lines
            .iter()
            .find(|l| l.to_ascii_uppercase().contains(" CREATE "))
            .expect("a CREATE must have gone out");
        assert!(
            create.ends_with("CREATE \"&BB8EQAQ+BDUEOgRCBEs-\""),
            "mailbox names go on the wire in modified UTF-7 (RFC 3501 5.1.3), got {create:?}"
        );
        let rename = lines
            .iter()
            .find(|l| l.to_ascii_uppercase().contains(" RENAME "))
            .expect("a RENAME must have gone out");
        assert!(
            rename.ends_with(
                "RENAME \"&BB8EQAQ+BDUEOgRCBEs-/Ideas\" \"&BB8EQAQ+BDUEOgRCBEs-/&BBgENAQ1BDg-\""
            ),
            "both paths must reach the server exactly as given, got {rename:?}"
        );
        assert!(
            !rename.contains("&-BB8"),
            "an escaped ampersand means the prefix was encoded twice: {rename:?}"
        );
    }

    #[test]
    fn condstore_is_enabled_so_select_reports_highestmodseq() {
        let (port, log) = spawn_recording_server(true);
        thread::sleep(Duration::from_millis(30));
        let mut session = ImapSession::connect(&form(port), ImapAuth::Login("x".into())).unwrap();
        assert!(session.capabilities().unwrap().condstore);

        let selected = session.select("INBOX").unwrap();

        let lines = log.lock().unwrap().clone();
        assert_eq!(
            selected.highest_modseq,
            Some(999),
            "a CONDSTORE-enabling command must run before SELECT, sent: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.to_ascii_uppercase().contains("ENABLE CONDSTORE")),
            "ENABLE CONDSTORE must go out once, in authenticated state, sent: {lines:?}"
        );

        // Once per session, not before every SELECT.
        session.select("INBOX").unwrap();
        let lines = log.lock().unwrap().clone();
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.to_ascii_uppercase().contains("ENABLE CONDSTORE"))
                .count(),
            1,
            "ENABLE must not repeat on every SELECT, sent: {lines:?}"
        );
    }

    #[test]
    fn condstore_is_not_enabled_when_the_server_never_advertised_it() {
        let (port, log) = spawn_recording_server(false);
        thread::sleep(Duration::from_millis(30));
        let mut session = ImapSession::connect(&form(port), ImapAuth::Login("x".into())).unwrap();
        assert!(!session.capabilities().unwrap().condstore);

        let selected = session.select("INBOX").unwrap();
        assert_eq!(selected.highest_modseq, None);
        let lines = log.lock().unwrap().clone();
        assert!(
            !lines
                .iter()
                .any(|l| l.to_ascii_uppercase().contains("ENABLE")),
            "ENABLE must not be sent to a server that never advertised CONDSTORE, sent: {lines:?}"
        );
    }

    #[test]
    fn select_reports_uidvalidity_uidnext_modseq() {
        let port = spawn_full_server();
        thread::sleep(Duration::from_millis(30));
        let mut session = ImapSession::connect(&form(port), ImapAuth::Login("x".into())).unwrap();
        let caps = session.capabilities().unwrap();
        assert!(caps.condstore);
        assert!(caps.idle);
        assert!(caps.can_move);

        let selected = session.select("INBOX").unwrap();
        assert_eq!(selected.uidvalidity, 123456);
        assert_eq!(selected.uidnext, 42);
        assert_eq!(selected.exists, 3);
        assert_eq!(selected.highest_modseq, Some(999));
        session.logout().unwrap();
    }

    #[test]
    fn list_maps_special_use_to_folder_kind() {
        let port = spawn_full_server();
        thread::sleep(Duration::from_millis(30));
        let mut session = ImapSession::connect(&form(port), ImapAuth::Login("x".into())).unwrap();
        let folders = session.list_folders().unwrap();
        let inbox = folders.iter().find(|f| f.name == "INBOX").unwrap();
        assert_eq!(inbox.kind, FolderKind::Inbox);
        let sent = folders.iter().find(|f| f.name == "Sent Items").unwrap();
        assert_eq!(sent.kind, FolderKind::Sent);
        let gmail = folders.iter().find(|f| f.name == "[Gmail]").unwrap();
        assert_eq!(gmail.kind, FolderKind::Custom);
        assert!(gmail.no_select);
        assert!(gmail.has_children);
    }

    #[test]
    fn uid_fetch_parses_literal_header_block() {
        let port = spawn_full_server();
        thread::sleep(Duration::from_millis(30));
        let mut session = ImapSession::connect(&form(port), ImapAuth::Login("x".into())).unwrap();
        let headers = session
            .uid_fetch_headers(UidRange::bounded(1, 100))
            .unwrap();
        assert_eq!(headers.len(), 1);
        let h = &headers[0];
        assert_eq!(h.uid, 7);
        assert_eq!(h.flags, vec!["\\Seen".to_string()]);
        assert_eq!(h.size_bytes, Some(1234));
        assert_eq!(h.subject.as_deref(), Some("Hi there"));
        assert_eq!(h.from.as_deref(), Some("a@b.com"));
        assert_eq!(h.cc.as_deref(), Some("copy@example.com"));
        assert_eq!(h.message_id.as_deref(), Some("<abc@x>"));
        assert!(h.internaldate.is_some());
    }

    #[test]
    fn append_message_waits_for_continuation_and_writes_exact_literal() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            write!(writer, "* OK fake ready\r\n").unwrap();
            writer.flush().unwrap();
            let mut login = String::new();
            reader.read_line(&mut login).unwrap();
            let login_tag = login.split_whitespace().next().unwrap().to_string();
            write!(writer, "{login_tag} OK logged in\r\n").unwrap();
            writer.flush().unwrap();
            let mut append = String::new();
            reader.read_line(&mut append).unwrap();
            let tag = append.split_whitespace().next().unwrap().to_string();
            assert!(append.contains("APPEND \"Sent Items\" (\\Seen) {12}"));
            write!(writer, "+ send literal\r\n").unwrap();
            writer.flush().unwrap();
            let mut literal = vec![0_u8; 14];
            reader.read_exact(&mut literal).unwrap();
            tx.send(literal).unwrap();
            write!(writer, "{tag} OK [APPENDUID 1 9] appended\r\n").unwrap();
            writer.flush().unwrap();
        });
        let mut session = ImapSession::connect(&form(port), ImapAuth::Login("x".into())).unwrap();
        let appended = session
            .append_message("Sent Items", &["\\Seen"], b"hello\r\nworld")
            .unwrap();
        assert_eq!(appended, Some(9));
        assert_eq!(rx.recv().unwrap(), b"hello\r\nworld\r\n");
    }

    #[test]
    fn truncated_response_is_an_error_not_a_panic() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            write!(writer, "* OK fake ready\r\n").unwrap();
            writer.flush().unwrap();
            let mut line = String::new();
            let _ = reader.read_line(&mut line); // LOGIN
            let tag = line.split_whitespace().next().unwrap_or("*").to_string();
            write!(writer, "{tag} OK logged in\r\n").unwrap();
            writer.flush().unwrap();
            // Next command (SELECT): claim a literal is coming, then close
            // the socket before sending it.
            let mut cmd = String::new();
            let _ = reader.read_line(&mut cmd);
            write!(writer, "* OK [UIDVALIDITY 1] x {{500}}\r\n").unwrap();
            writer.flush().unwrap();
            // Drop the connection instead of sending the literal payload.
        });
        thread::sleep(Duration::from_millis(30));
        let mut session = ImapSession::connect(&form(port), ImapAuth::Login("x".into())).unwrap();
        let err = session.select("INBOX").unwrap_err();
        match err {
            ConnectError::Network { .. } => {}
            other => panic!("expected a network error, got {other:?}"),
        }
    }

    #[test]
    fn malformed_uid_in_fetch_line_is_skipped_not_a_panic() {
        // No live server needed: exercise the pure parser directly.
        let garbled = "* 1 FETCH (UID FLAGS (\\Seen)"; // missing UID value and closing paren
        assert!(parse_fetch_line(garbled).is_none());
        let empty = "";
        assert!(parse_fetch_line(empty).is_none());
        let unbalanced = "* LIST (\\HasNoChildren";
        assert!(parse_list_line(unbalanced).is_none());
    }

    #[test]
    fn parse_fetch_line_reads_x_gm_thrid() {
        let line = "* 1 FETCH (UID 7 X-GM-THRID 1461918615937847476 FLAGS (\\Seen))";
        let h = parse_fetch_line(line).unwrap();
        assert_eq!(h.uid, 7);
        assert_eq!(h.gm_thrid.as_deref(), Some("1461918615937847476"));
    }

    /// Captures the exact `UID FETCH` command line. `advertise_gm` controls
    /// whether CAPABILITY lists `X-GM-EXT-1`.
    fn spawn_fetch_capture_server(advertise_gm: bool) -> (u16, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            write!(writer, "* OK fake ready\r\n").unwrap();
            writer.flush().unwrap();
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let upper = line.to_ascii_uppercase();
                let tag = line.split_whitespace().next().unwrap_or("*").to_string();
                if upper.contains(" LOGIN ") {
                    write!(writer, "{tag} OK logged in\r\n").unwrap();
                } else if upper.contains(" CAPABILITY") {
                    if advertise_gm {
                        write!(
                            writer,
                            "* CAPABILITY IMAP4rev1 X-GM-EXT-1\r\n{tag} OK CAPABILITY completed\r\n"
                        )
                        .unwrap();
                    } else {
                        write!(
                            writer,
                            "* CAPABILITY IMAP4rev1\r\n{tag} OK CAPABILITY completed\r\n"
                        )
                        .unwrap();
                    }
                } else if upper.contains("UID FETCH") {
                    let _ = tx.send(line.trim().to_string());
                    let header = "Subject: Hi\r\nMessage-ID: <abc@x>\r\n";
                    if advertise_gm {
                        write!(
                            writer,
                            "* 1 FETCH (UID 7 X-GM-THRID 99 FLAGS (\\Seen) BODY[HEADER.FIELDS (MESSAGE-ID)] {{{}}}\r\n",
                            header.len()
                        )
                        .unwrap();
                    } else {
                        write!(
                            writer,
                            "* 1 FETCH (UID 7 FLAGS (\\Seen) BODY[HEADER.FIELDS (MESSAGE-ID)] {{{}}}\r\n",
                            header.len()
                        )
                        .unwrap();
                    }
                    write!(writer, "{header}").unwrap();
                    write!(writer, ")\r\n").unwrap();
                    write!(writer, "{tag} OK UID FETCH completed\r\n").unwrap();
                } else if upper.contains("LOGOUT") {
                    write!(writer, "* BYE\r\n{tag} OK LOGOUT\r\n").unwrap();
                    break;
                } else {
                    write!(writer, "{tag} BAD unknown\r\n").unwrap();
                }
                writer.flush().unwrap();
            }
        });
        (port, rx)
    }

    #[test]
    fn generic_fetch_does_not_name_x_gm_thrid() {
        let (port, rx) = spawn_fetch_capture_server(false);
        thread::sleep(Duration::from_millis(30));
        let mut session = ImapSession::connect(&form(port), ImapAuth::Login("x".into())).unwrap();
        let _ = session.uid_fetch_headers(UidRange::bounded(1, 1)).unwrap();
        let cmd = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            !cmd.to_ascii_uppercase().contains("X-GM-THRID"),
            "generic IMAP must not FETCH X-GM-THRID: {cmd}"
        );
        assert!(
            cmd.to_ascii_uppercase().contains(" TO CC SUBJECT "),
            "Reply all needs Cc in the ordinary header fetch: {cmd}"
        );
    }

    #[test]
    fn gmail_fetch_names_x_gm_thrid_and_parses_it() {
        let (port, rx) = spawn_fetch_capture_server(true);
        thread::sleep(Duration::from_millis(30));
        let mut session = ImapSession::connect(&form(port), ImapAuth::Login("x".into())).unwrap();
        let headers = session.uid_fetch_headers(UidRange::bounded(1, 1)).unwrap();
        let cmd = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            cmd.to_ascii_uppercase().contains("X-GM-THRID"),
            "X-GM-EXT-1 must FETCH X-GM-THRID: {cmd}"
        );
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].gm_thrid.as_deref(), Some("99"));
    }

    /// Fake server for the `BODY.PEEK[]` tests: forwards the exact `UID
    /// FETCH` command line it received on `tx` (so a test can assert on
    /// what the client actually sent on the wire), then replies with one
    /// FETCH literal of `body`. The response is labelled `BODY[]` in the
    /// reply -- real servers echo back whatever section was requested, and
    /// this test cares about what the *client* asked for, not what a fake
    /// server happens to label its reply.
    fn spawn_body_peek_server(body: Vec<u8>) -> (u16, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            write!(writer, "* OK fake ready\r\n").unwrap();
            writer.flush().unwrap();
            let mut line = String::new();
            // LOGIN
            line.clear();
            reader.read_line(&mut line).unwrap();
            let tag = line.split_whitespace().next().unwrap().to_string();
            write!(writer, "{tag} OK logged in\r\n").unwrap();
            writer.flush().unwrap();
            // UID FETCH -- capture the exact command line sent.
            line.clear();
            reader.read_line(&mut line).unwrap();
            tx.send(line.clone()).unwrap();
            let tag2 = line.split_whitespace().next().unwrap().to_string();
            write!(writer, "* 1 FETCH (UID 9 BODY[] {{{}}}\r\n", body.len()).unwrap();
            writer.write_all(&body).unwrap();
            write!(writer, ")\r\n").unwrap();
            write!(writer, "{tag2} OK UID FETCH completed\r\n").unwrap();
            writer.flush().unwrap();
        });
        (port, rx)
    }

    /// Answers three bodies out of order, with untagged chatter mixed in
    /// and one UID the client never asked for -- the shapes a real server
    /// is allowed to produce and the batch has to survive.
    fn spawn_batch_body_server(
        bodies: Vec<(u32, Vec<u8>)>,
    ) -> (u16, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            write!(writer, "* OK fake ready\r\n").unwrap();
            writer.flush().unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let tag = line.split_whitespace().next().unwrap().to_string();
            write!(writer, "{tag} OK logged in\r\n").unwrap();
            writer.flush().unwrap();

            line.clear();
            reader.read_line(&mut line).unwrap();
            tx.send(line.clone()).unwrap();
            let tag2 = line.split_whitespace().next().unwrap().to_string();
            write!(writer, "* 41 EXISTS\r\n").unwrap();
            for (seq, (uid, body)) in bodies.iter().enumerate() {
                write!(
                    writer,
                    "* {} FETCH (UID {uid} RFC822.SIZE {} BODY[] {{{}}}\r\n",
                    seq + 1,
                    body.len(),
                    body.len()
                )
                .unwrap();
                writer.write_all(body).unwrap();
                write!(writer, ")\r\n").unwrap();
            }
            write!(writer, "{tag2} OK UID FETCH completed\r\n").unwrap();
            writer.flush().unwrap();
        });
        (port, rx)
    }

    /// T-024 batched: one command for the whole set, and every body lands
    /// on the UID the server named -- not on the position it arrived in.
    #[test]
    fn uid_fetch_bodies_peek_is_one_command_and_pairs_by_uid() {
        let bodies = vec![
            (31u32, b"Subject: three\r\n\r\nc".to_vec()),
            (11u32, b"Subject: one\r\n\r\na".to_vec()),
            (21u32, b"Subject: two\r\n\r\nbb".to_vec()),
        ];
        let (port, rx) = spawn_batch_body_server(bodies.clone());
        thread::sleep(Duration::from_millis(30));
        let mut session = ImapSession::connect(&form(port), ImapAuth::Login("x".into())).unwrap();

        let got = session.uid_fetch_bodies_peek(&[11, 21, 31]).unwrap();

        let cmd_line = rx.recv().unwrap();
        let upper = cmd_line.to_ascii_uppercase();
        assert!(
            upper.contains("UID FETCH 11,21,31"),
            "the set must go out as one command, got: {cmd_line:?}"
        );
        assert!(
            upper.contains("BODY.PEEK[]"),
            "warming a body must not set \\Seen either, got: {cmd_line:?}"
        );

        let mut got_sorted = got;
        got_sorted.sort_by_key(|(uid, _)| *uid);
        let mut want = bodies;
        want.sort_by_key(|(uid, _)| *uid);
        assert_eq!(
            got_sorted, want,
            "each body must come back on the UID the server named"
        );
    }

    #[test]
    fn uid_fetch_body_peek_sends_peek_not_bare_body() {
        let body = b"Subject: hi\r\n\r\nhello".to_vec();
        let (port, rx) = spawn_body_peek_server(body.clone());
        thread::sleep(Duration::from_millis(30));
        let mut session = ImapSession::connect(&form(port), ImapAuth::Login("x".into())).unwrap();

        let fetched = session.uid_fetch_body_peek(9).unwrap();
        assert_eq!(fetched, body);

        let cmd_line = rx.recv().unwrap();
        let upper = cmd_line.to_ascii_uppercase();
        assert!(
            upper.contains("BODY.PEEK[]"),
            "expected BODY.PEEK[] on the wire, got: {cmd_line:?}"
        );
        assert!(
            !upper.contains("BODY[]"),
            "must never send bare BODY[] -- it sets \\Seen as a side effect: {cmd_line:?}"
        );
    }

    #[test]
    fn uid_fetch_body_peek_reads_the_full_literal_including_embedded_crlf_and_binary() {
        // A decoy line inside the literal that looks exactly like a tagged
        // completion, plus embedded CRLFs and non-UTF-8 bytes -- none of
        // this should cause the reader to stop early or corrupt the bytes,
        // since it reads exactly the declared literal length via
        // `read_exact`, never by scanning for a line terminator.
        let mut body =
            b"Subject: hi\r\n\r\nline one\r\nS7 OK not a real tag\r\nline two\r\n".to_vec();
        body.extend_from_slice(&[0u8, 1, 2, 0xff, 0xfe, b'\r', b'\n', 0x80]);
        let (port, _rx) = spawn_body_peek_server(body.clone());
        thread::sleep(Duration::from_millis(30));
        let mut session = ImapSession::connect(&form(port), ImapAuth::Login("x".into())).unwrap();

        let fetched = session.uid_fetch_body_peek(9).unwrap();
        assert_eq!(fetched, body, "literal must be read byte-for-byte, in full");
    }

    #[test]
    fn uid_fetch_body_peek_error_never_carries_message_content() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            write!(writer, "* OK fake ready\r\n").unwrap();
            writer.flush().unwrap();
            let mut line = String::new();
            line.clear();
            reader.read_line(&mut line).unwrap();
            let tag = line.split_whitespace().next().unwrap().to_string();
            write!(writer, "{tag} OK logged in\r\n").unwrap();
            writer.flush().unwrap();
            line.clear();
            reader.read_line(&mut line).unwrap();
            let tag2 = line.split_whitespace().next().unwrap().to_string();
            write!(writer, "{tag2} NO super secret subject line leaked\r\n").unwrap();
            writer.flush().unwrap();
        });
        thread::sleep(Duration::from_millis(30));
        let mut session = ImapSession::connect(&form(port), ImapAuth::Login("x".into())).unwrap();
        let err = session.uid_fetch_body_peek(9).unwrap_err();
        match err {
            ConnectError::Network { message, details } => {
                // Unlike `uid_fetch_headers`/`run_ok`, which fold the raw
                // (sanitized) server line into the error for debuggability,
                // a body-fetch failure carries only the UID and a fixed
                // description -- never any server-supplied text, since
                // there is no way to be sure a server's rejection line
                // could never echo back message content.
                assert!(!message.contains("secret subject line"));
                assert!(details.is_none_or(|d| !d.contains("secret subject line")));
            }
            other => panic!("expected a network error, got {other:?}"),
        }
    }

    /// Mirrors `spawn_body_peek_server`, but returns an encoded MIME section
    /// rather than a whole RFC822 message. Keeping the payload only in this
    /// test helper makes the client-side assertion meaningful: the public
    /// method must write it to disk through its streaming path, not borrow a
    /// body cache or reuse `uid_fetch_body_peek`.
    fn spawn_attachment_section_server(
        encoded: Vec<u8>,
    ) -> (u16, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            write!(writer, "* OK fake ready\r\n").unwrap();
            writer.flush().unwrap();

            let mut line = String::new();
            reader.read_line(&mut line).unwrap(); // LOGIN
            let login_tag = line.split_whitespace().next().unwrap().to_string();
            write!(writer, "{login_tag} OK logged in\r\n").unwrap();
            writer.flush().unwrap();

            line.clear();
            reader.read_line(&mut line).unwrap(); // UID FETCH
            tx.send(line.clone()).unwrap();
            let fetch_tag = line.split_whitespace().next().unwrap().to_string();
            write!(
                writer,
                "* 1 FETCH (UID 9 BODY[1.2] {{{}}}\r\n",
                encoded.len()
            )
            .unwrap();
            writer.write_all(&encoded).unwrap();
            write!(writer, ")\r\n{fetch_tag} OK UID FETCH completed\r\n").unwrap();
            writer.flush().unwrap();

            line.clear();
            if reader.read_line(&mut line).unwrap() > 0
                && line.to_ascii_uppercase().contains("LOGOUT")
            {
                let logout_tag = line.split_whitespace().next().unwrap().to_string();
                write!(writer, "* BYE\r\n{logout_tag} OK LOGOUT\r\n").unwrap();
                writer.flush().unwrap();
            }
        });
        (port, rx)
    }

    #[test]
    fn uid_fetch_section_streams_decoded_attachment_to_file_with_peek() {
        let (port, rx) = spawn_attachment_section_server(b"aGVs\r\nbG8=".to_vec());
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("report.pdf");
        thread::sleep(Duration::from_millis(30));
        let mut session = ImapSession::connect(&form(port), ImapAuth::Login("x".into())).unwrap();

        let bytes = session
            .uid_fetch_section_to_file(
                9,
                "1.2",
                TransferEncoding::Base64,
                &destination,
                AttachmentFetchLimits {
                    max_wire_bytes: 1024,
                    max_decoded_bytes: 1024,
                },
            )
            .unwrap();
        assert_eq!(bytes, 5);
        assert_eq!(std::fs::read(&destination).unwrap(), b"hello");

        let command = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let upper = command.to_ascii_uppercase();
        assert!(
            upper.contains("BODY.PEEK[1.2]"),
            "wrong command: {command:?}"
        );
        assert!(
            !upper.contains("BODY[1.2]"),
            "must not fetch an attachment with bare BODY: {command:?}"
        );
        session.logout().unwrap();
    }

    #[test]
    fn attachment_download_failure_drains_literal_before_logout() {
        let (port, _rx) = spawn_attachment_section_server(b"aGVsbG8=".to_vec());
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("too-large.bin");
        thread::sleep(Duration::from_millis(30));
        let mut session = ImapSession::connect(&form(port), ImapAuth::Login("x".into())).unwrap();

        let err = session
            .uid_fetch_section_to_file(
                9,
                "1",
                TransferEncoding::Base64,
                &destination,
                AttachmentFetchLimits {
                    max_wire_bytes: 1024,
                    max_decoded_bytes: 1,
                },
            )
            .unwrap_err();
        assert!(matches!(err, ConnectError::Network { .. }));
        assert!(!destination.exists());
        assert!(
            session.logout().is_ok(),
            "a rejected cache write must not leave unread literal bytes before the next command"
        );
    }

    /// Promises a body literal of `declared` bytes and hangs up after
    /// `sent` of them (T-070's "disconnect during body fetch", T-112).
    fn spawn_body_cutoff_server(declared: usize, sent: &'static [u8]) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            write!(writer, "* OK fake ready\r\n").unwrap();
            writer.flush().unwrap();

            let mut line = String::new();
            reader.read_line(&mut line).unwrap(); // LOGIN
            let login_tag = line.split_whitespace().next().unwrap().to_string();
            write!(writer, "{login_tag} OK logged in\r\n").unwrap();
            writer.flush().unwrap();

            line.clear();
            reader.read_line(&mut line).unwrap(); // UID FETCH
            write!(writer, "* 1 FETCH (UID 9 BODY[] {{{declared}}}\r\n").unwrap();
            writer.write_all(sent).unwrap();
            writer.flush().unwrap();
            // Hang up in the middle of the body.
        });
        port
    }

    #[test]
    fn a_body_cut_off_mid_literal_is_an_error_not_half_a_message() {
        let port = spawn_body_cutoff_server(4096, b"Subject: half\r\n\r\nThe beginning of");
        thread::sleep(Duration::from_millis(30));
        let mut session = ImapSession::connect(&form(port), ImapAuth::Login("x".into())).unwrap();

        // Half a message must never come back as `Ok`: `Core::store_body`
        // would cache it, and the owner would read a truncated letter with no
        // way to tell it apart from a short one.
        let err = session.uid_fetch_body_peek(9).unwrap_err();
        assert!(matches!(err, ConnectError::Network { .. }), "got {err:?}");
    }

    /// Answers a header `UID FETCH` with some of the rows it promised and
    /// then closes the socket mid-response (T-070's "unstable network ...
    /// disconnect during header fetch", T-112).
    fn spawn_header_cutoff_server() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            write!(writer, "* OK fake ready\r\n").unwrap();
            writer.flush().unwrap();

            let mut line = String::new();
            reader.read_line(&mut line).unwrap(); // LOGIN
            let login_tag = line.split_whitespace().next().unwrap().to_string();
            write!(writer, "{login_tag} OK logged in\r\n").unwrap();
            writer.flush().unwrap();

            line.clear();
            reader.read_line(&mut line).unwrap(); // UID FETCH
            write!(
                writer,
                "* 1 FETCH (UID 11 FLAGS (\\Seen) INTERNALDATE \"01-Jan-2026 00:00:00 +0000\" \
                 RFC822.SIZE 10 BODY[HEADER.FIELDS (SUBJECT)] {{18}}\r\nSubject: first\r\n\r\n)\r\n"
            )
            .unwrap();
            write!(writer, "* 2 FETCH (UID 12 FLAGS (\\Seen) INTERNAL").unwrap();
            writer.flush().unwrap();
            // Hang up half way through the second row, before the tagged OK.
        });
        port
    }

    #[test]
    fn headers_cut_off_mid_response_are_an_error_not_a_short_page() {
        let port = spawn_header_cutoff_server();
        thread::sleep(Duration::from_millis(30));
        let mut session = ImapSession::connect(&form(port), ImapAuth::Login("x".into())).unwrap();

        // The rows that did arrive must not come back as a successful, merely
        // shorter page: the sync engine reads a short page as "this UID range
        // holds nothing more" and would leave a permanent hole in the folder.
        let err = session
            .uid_fetch_headers(UidRange::bounded(11, 12))
            .unwrap_err();
        assert!(matches!(err, ConnectError::Network { .. }), "got {err:?}");
    }

    /// Announces a literal of `declared` bytes, sends `sent` of them and
    /// then closes the socket: an attachment download cut off by the
    /// network (T-070's "unstable network" line, T-112).
    fn spawn_attachment_cutoff_server(declared: usize, sent: Vec<u8>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            write!(writer, "* OK fake ready\r\n").unwrap();
            writer.flush().unwrap();

            let mut line = String::new();
            reader.read_line(&mut line).unwrap(); // LOGIN
            let login_tag = line.split_whitespace().next().unwrap().to_string();
            write!(writer, "{login_tag} OK logged in\r\n").unwrap();
            writer.flush().unwrap();

            line.clear();
            reader.read_line(&mut line).unwrap(); // UID FETCH
            write!(writer, "* 1 FETCH (UID 9 BODY[1] {{{declared}}}\r\n").unwrap();
            writer.write_all(&sent).unwrap();
            writer.flush().unwrap();
            // Hang up in the middle of the literal.
        });
        port
    }

    #[test]
    fn an_attachment_cut_off_mid_download_leaves_nothing_behind() {
        // Valid base64 as far as it goes, so nothing but the early EOF can
        // make this fail: "aGVsbG8=" would decode cleanly on its own.
        let port = spawn_attachment_cutoff_server(400, b"aGVsbG8=".to_vec());
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("report.pdf");
        thread::sleep(Duration::from_millis(30));
        let mut session = ImapSession::connect(&form(port), ImapAuth::Login("x".into())).unwrap();

        let err = session
            .uid_fetch_section_to_file(
                9,
                "1",
                TransferEncoding::Base64,
                &destination,
                AttachmentFetchLimits {
                    max_wire_bytes: 4096,
                    max_decoded_bytes: 4096,
                },
            )
            .unwrap_err();
        assert!(matches!(err, ConnectError::Network { .. }));
        // The cache path must stay empty: `download_one_attachment` treats an
        // existing file there as a complete download and would mark the
        // truncated remains as cached.
        assert!(
            !destination.exists(),
            "a half-downloaded attachment must not appear at the cache path"
        );
        assert!(
            !dir.path().join("report.pdf.part").exists(),
            "the partial file must be removed, not left to sit outside the cache budget"
        );
    }

    #[test]
    fn the_same_attachment_downloads_whole_after_a_cut_off_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("report.pdf");

        let cutoff = spawn_attachment_cutoff_server(400, b"aGVsbG8=".to_vec());
        thread::sleep(Duration::from_millis(30));
        let mut broken = ImapSession::connect(&form(cutoff), ImapAuth::Login("x".into())).unwrap();
        let limits = AttachmentFetchLimits {
            max_wire_bytes: 4096,
            max_decoded_bytes: 4096,
        };
        assert!(broken
            .uid_fetch_section_to_file(9, "1", TransferEncoding::Base64, &destination, limits)
            .is_err());

        let (port, _rx) = spawn_attachment_section_server(b"aGVsbG8=".to_vec());
        thread::sleep(Duration::from_millis(30));
        let mut session = ImapSession::connect(&form(port), ImapAuth::Login("x".into())).unwrap();
        let bytes = session
            .uid_fetch_section_to_file(9, "1.2", TransferEncoding::Base64, &destination, limits)
            .unwrap();
        assert_eq!(bytes, 5);
        assert_eq!(std::fs::read(&destination).unwrap(), b"hello");
    }

    #[test]
    fn attachment_section_grammar_rejects_injection_and_non_payload_items() {
        for section in [
            "",
            "0",
            "01",
            "1.",
            ".1",
            "1..2",
            "1] UID STORE 1 +FLAGS (\\Seen)",
            "HEADER.FIELDS (SUBJECT)",
        ] {
            assert!(
                !is_valid_body_section(section),
                "section must be rejected: {section:?}"
            );
        }
        for section in ["TEXT", "1", "2.1", "12.34.56"] {
            assert!(
                is_valid_body_section(section),
                "section must be accepted: {section:?}"
            );
        }
    }
}
