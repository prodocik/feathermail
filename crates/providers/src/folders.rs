//! Turns a live `LIST` walk into [`DiscoveredFolder`]s Core can store
//! (T-077).
//!
//! `ImapSession::list_folders` already resolves each mailbox to a
//! [`FolderKind`] via SPECIAL-USE (RFC 6154); this module is the one place
//! that decides how a [`FolderListing`] becomes the smaller, IMAP-agnostic
//! shape `feathermail_core::remote::Core::sync_folders` accepts, keeping
//! attribute strings, the hierarchy delimiter, and `\Noselect` out of
//! `feathermail-core` (D9). The hierarchy delimiter is also the only place
//! `parent_remote_id` can be computed from, so it is derived here too and
//! handed over as an opaque `remote_id` string -- `feathermail-core` never
//! learns what a delimiter is, it just links a child's `parent_remote_id`
//! to another folder's `remote_id` (see `Core::sync_folders`'s doc
//! comment).

use base64::{engine::general_purpose::STANDARD, Engine as _};
use feathermail_core::model::FolderKind;
use feathermail_core::remote::DiscoveredFolder;

use crate::session::FolderListing;

/// Converts one `LIST` walk's results to the folders Core should learn
/// about.
///
/// A `\Noselect` entry (a hierarchy container with no mailbox behind it,
/// e.g. IMAP's `[Gmail]`) is dropped: there is nothing to select, sync, or
/// file mail under, so it would only ever show up as a permanently-empty
/// dead folder in the sidebar.
///
/// For a recognized system kind the label is the canonical
/// [`FolderKind::default_label`] rather than the server's own mailbox
/// name, so a discovered `INBOX`/`\Archive`/… mailbox lines up by name
/// with the placeholder row `Core::add_account` (Inbox) or an earlier
/// `sync_folders` call already wrote under that label — matching a
/// remote_id-less placeholder by name + kind is how `sync_folders` adopts
/// it (`Core::sync_one_folder` step 2; since T-079 `folders` is uniqued on
/// `(account_id, remote_id)`, so the name is an adoption hint, not the
/// identity). For [`FolderKind::Custom`]
/// there is no canonical label, so the display name is the mailbox's own
/// last path segment (split on its own hierarchy delimiter, when the
/// server reports one), so a nested mailbox like `Team/Ideas` shows up as
/// "Ideas" rather than the full server path.
pub fn discovered_folders(listings: &[FolderListing]) -> Vec<DiscoveredFolder> {
    listings
        .iter()
        .filter(|listing| !listing.no_select)
        .map(|listing| DiscoveredFolder {
            remote_id: listing.name.clone(),
            kind: listing.kind,
            label: folder_label(listing),
            parent_remote_id: parent_remote_id(listing),
            delimiter: listing.delimiter,
        })
        .collect()
}

fn folder_label(listing: &FolderListing) -> String {
    if listing.kind != FolderKind::Custom {
        return listing.kind.default_label().to_string();
    }
    let raw_label = match listing.delimiter {
        Some(delim) => listing
            .name
            .rsplit(delim)
            .next()
            .filter(|segment| !segment.is_empty())
            .unwrap_or(&listing.name)
            .to_string(),
        None => listing.name.clone(),
    };
    decode_modified_utf7(&raw_label)
}

/// Decode an IMAP mailbox display name from modified UTF-7 (RFC 3501 §5.1.3)
/// while keeping `FolderListing::name` untouched for protocol operations.
///
/// A malformed encoded run stays verbatim rather than losing a mailbox from
/// the sidebar. The IMAP wire identifier is intentionally never decoded:
/// `SELECT`, `MOVE`, and parent matching must receive the exact string the
/// server advertised.
fn decode_modified_utf7(input: &str) -> String {
    let mut decoded = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(start) = rest.find('&') {
        decoded.push_str(&rest[..start]);
        let encoded = &rest[start + 1..];
        let Some(end) = encoded.find('-') else {
            decoded.push_str(&rest[start..]);
            return decoded;
        };
        let token = &encoded[..end];
        let wire = &rest[..start + end + 2];
        if token.is_empty() {
            // "&-" is the literal ampersand escape in modified UTF-7.
            decoded.push('&');
        } else if let Some(value) = decode_modified_utf7_token(token) {
            decoded.push_str(&value);
        } else {
            decoded.push_str(wire);
        }
        rest = &encoded[end + 1..];
    }
    decoded.push_str(rest);
    decoded
}

fn decode_modified_utf7_token(token: &str) -> Option<String> {
    let mut base64 = token.replace(',', "/");
    base64.extend(std::iter::repeat_n('=', (4 - base64.len() % 4) % 4));
    let bytes = STANDARD.decode(base64).ok()?;
    let (pairs, remainder) = bytes.as_chunks::<2>();
    if !remainder.is_empty() {
        return None;
    }
    let words = pairs
        .iter()
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    String::from_utf16(&words).ok()
}

/// The `remote_id` of `listing`'s parent mailbox, if its own name has one
/// under the server's hierarchy delimiter (e.g. `Some("Team")` for
/// `Team/Ideas`). `None` when the server reported no delimiter for this
/// listing, or the name has no delimiter-separated prefix (a top-level
/// mailbox). `Core::sync_folders` looks this up as just another
/// `remote_id`, so whether the parent mailbox actually exists locally
/// (e.g. it wasn't filtered out as `\Noselect`) is entirely its call, not
/// this function's.
fn parent_remote_id(listing: &FolderListing) -> Option<String> {
    let delim = listing.delimiter?;
    let (parent, _leaf) = listing.name.rsplit_once(delim)?;
    if parent.is_empty() {
        None
    } else {
        Some(parent.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(
        name: &str,
        delimiter: Option<char>,
        no_select: bool,
        kind: FolderKind,
    ) -> FolderListing {
        FolderListing {
            name: name.to_string(),
            delimiter,
            has_children: false,
            no_select,
            kind,
        }
    }

    #[test]
    fn system_kinds_use_the_canonical_label_not_the_server_name() {
        let listings = vec![listing("INBOX", Some('/'), false, FolderKind::Inbox)];
        let discovered = discovered_folders(&listings);
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].remote_id, "INBOX");
        assert_eq!(discovered[0].label, "Inbox");
        assert_eq!(discovered[0].kind, FolderKind::Inbox);
    }

    #[test]
    fn custom_kind_label_is_the_last_path_segment() {
        let listings = vec![listing("Team/Ideas", Some('/'), false, FolderKind::Custom)];
        let discovered = discovered_folders(&listings);
        assert_eq!(discovered[0].remote_id, "Team/Ideas");
        assert_eq!(discovered[0].label, "Ideas");
    }

    #[test]
    fn custom_kind_with_no_delimiter_uses_the_whole_name() {
        let listings = vec![listing("Notes", None, false, FolderKind::Custom)];
        let discovered = discovered_folders(&listings);
        assert_eq!(discovered[0].label, "Notes");
    }

    #[test]
    fn custom_kind_decodes_modified_utf7_for_display_only() {
        let raw = "&BBgEQQRFBD4ENARPBEkEOAQ1-"; // "Исходящие"
        let listings = vec![listing(raw, Some('/'), false, FolderKind::Custom)];
        let discovered = discovered_folders(&listings);
        assert_eq!(discovered[0].remote_id, raw);
        assert_eq!(discovered[0].label, "Исходящие");
    }

    #[test]
    fn modified_utf7_literal_ampersand_and_malformed_token_stay_readable() {
        assert_eq!(decode_modified_utf7("R&-D &- Notes"), "R&D & Notes");
        assert_eq!(decode_modified_utf7("&not-base64-"), "&not-base64-");
    }

    #[test]
    fn no_select_containers_are_dropped() {
        let listings = vec![
            listing("[Gmail]", Some('/'), true, FolderKind::Custom),
            listing("[Gmail]/All Mail", Some('/'), false, FolderKind::Custom),
        ];
        let discovered = discovered_folders(&listings);
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].remote_id, "[Gmail]/All Mail");
        assert_eq!(discovered[0].label, "All Mail");
    }

    #[test]
    fn trailing_delimiter_falls_back_to_the_full_name() {
        // A mailbox path ending in the delimiter has no non-empty last
        // segment; fall back to the full server name rather than an empty
        // label.
        let listings = vec![listing("Team/", Some('/'), false, FolderKind::Custom)];
        let discovered = discovered_folders(&listings);
        assert_eq!(discovered[0].label, "Team/");
    }

    #[test]
    fn nested_mailbox_carries_its_parent_remote_id() {
        let listings = vec![listing("Team/Ideas", Some('/'), false, FolderKind::Custom)];
        let discovered = discovered_folders(&listings);
        assert_eq!(discovered[0].parent_remote_id.as_deref(), Some("Team"));
    }

    #[test]
    fn top_level_mailbox_has_no_parent_remote_id() {
        let listings = vec![listing("Notes", Some('/'), false, FolderKind::Custom)];
        let discovered = discovered_folders(&listings);
        assert_eq!(discovered[0].parent_remote_id, None);
    }

    #[test]
    fn no_delimiter_means_no_parent_remote_id_even_with_slashes_in_the_name() {
        let listings = vec![listing("Team/Ideas", None, false, FolderKind::Custom)];
        let discovered = discovered_folders(&listings);
        assert_eq!(discovered[0].parent_remote_id, None);
    }
}
