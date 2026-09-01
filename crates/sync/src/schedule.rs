//! Sync priorities and adaptive cadence (T-023, ТЗ §40, §74–75).
//!
//! Pure scheduling policy: no sockets, no SQLite, no GTK, no threads, no
//! sleeping, and no clock of its own (`Instant::now`/`Date` are never
//! called here) — `now: i64` and every observed condition arrive as plain
//! arguments, so the policy is a deterministic function of its inputs and
//! is trivial to test without real time passing.
//!
//! ## What this module decides
//!
//! Given a set of folders (their role, and when each last synced/was last
//! attempted), what the user currently has open, and the device's
//! power/activity state, decide *which folder to sync next* and *in how
//! many seconds* — or that nothing is due yet. It does not talk to IMAP or
//! SQLite; [`crate::sync_folder`] (T-022) is what actually performs a sync
//! once this module says it's time.
//!
//! ## Local `FolderRole`
//!
//! `feathermail-sync` intentionally has no dependency on
//! `feathermail-core` or `feathermail-db` (see the crate-level doc comment:
//! the engine only talks to the world through the [`crate::MailboxSession`]
//! and [`crate::SyncStore`] traits). So this module cannot reference
//! whatever "folder kind" enum `core` defines — [`FolderRole`] below is a
//! narrower, scheduler-local view, declared fresh in this file. It only
//! distinguishes the roles that actually change scheduling *priority*;
//! anything else (custom/user folders, labels, anything unrecognized)
//! collapses into [`FolderRole::Other`]. The second half of T-023 (wiring
//! this into the real app) is expected to map `core`'s folder kind onto
//! this enum at the boundary, the same way T-022's second half maps
//! `FolderSyncState`.
//!
//! ## Policy in one paragraph
//!
//! Every folder has a base interval from its role, shortened if it is the
//! folder the user is actually looking at (open folder or the folder an
//! open thread lives in), lengthened by whatever power/activity conditions
//! apply. Independently, a folder that has been failing carries an error
//! backoff threshold measured from its *last attempt* (successful or not),
//! not from its last success — see [`FolderInput::last_attempt_at`] for why
//! that distinction is load-bearing. A folder is due only once *both*
//! thresholds have passed (the later of the two gates it). When more than
//! one folder is due at once, the higher-priority *tier* always wins over a
//! lower one, no matter how long the lower one has been waiting — this is
//! what makes "Sent never overtakes Inbox" true even when Sent is far more
//! overdue in absolute terms. See [`next_sync`] for the exact algorithm and
//! the (explicitly stated) assumption its starvation bound relies on.

/// Local mirror of the mailbox categories that affect scheduling priority.
///
/// Deliberately small and deliberately *not* `feathermail_core`'s folder
/// kind (see the module doc comment) — this crate must not depend on
/// `core`. Roles that behave the same for scheduling purposes are folded
/// together rather than each getting a variant, so this enum only grows
/// when a role actually needs a different cadence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FolderRole {
    /// The account's Inbox. Kept separate from `Other` because it is the
    /// one folder that matters even when the user isn't looking at it
    /// (ТЗ §74: new-mail notifications originate here).
    Inbox,
    /// Sent mail: almost always something *we* just did (and so already
    /// know about locally); background sync of Sent exists only to catch
    /// messages sent from another device.
    Sent,
    /// Drafts: same reasoning as Sent — cross-device edits are the only
    /// thing background sync is for here.
    Drafts,
    /// Archive: historical mail the user deliberately filed away.
    Archive,
    /// Trash: lowest-value background target — the user is unlikely to be
    /// waiting on anything that lands here.
    Trash,
    /// Spam/Junk: same rationale as Trash.
    Spam,
    /// Anything else: custom folders, provider labels, IMAP folders this
    /// crate doesn't recognize a special role for.
    Other,
}

/// What the user currently has open. Both fields name a folder id (the
/// same ids used in [`FolderInput::id`]); either or both may be unset.
///
/// Kept as two separate fields — not one — because the ticket calls out
/// "current/open folder" *and* "open thread" as two distinct signals (ТЗ
/// §74–75): a thread opened from a cross-folder view (e.g. search results)
/// can live in a folder that isn't the one currently displayed in the
/// sidebar. Both are treated identically by the scheduler: either one
/// names a folder the user is plausibly looking at right now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Focus<'a> {
    /// Folder id currently selected/displayed in the UI, if any.
    pub open_folder: Option<&'a str>,
    /// Folder id of the thread currently open for reading, if any.
    pub open_thread_folder: Option<&'a str>,
}

/// Power/activity conditions that slow down background sync (ТЗ §74–75:
/// "battery/idle снижает частоту").
///
/// The four fields are the conditions a desktop Linux mail client can
/// actually observe without a dedicated capability we don't have yet:
/// - `on_battery` — unplugged; every extra radio wakeup costs battery.
/// - `screen_locked_or_idle` — nobody is watching, so freshness matters
///   less than it does with an unlocked, attended screen.
/// - `app_backgrounded` — the window is minimized/not focused; the user
///   isn't looking at *this app* even if the machine is otherwise active.
/// - `network_metered` — background chatter has a direct cost to the user
///   (mobile hotspot, capped plan), independent of battery or idle state.
///
/// They're modeled as independent booleans rather than one "power saver"
/// flag because they can occur in any combination (e.g. plugged in but on
/// a metered hotspot) and each has a distinct, real justification for
/// slowing things down; see [`power_multiplier`] for how they combine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PowerState {
    pub on_battery: bool,
    pub screen_locked_or_idle: bool,
    pub app_backgrounded: bool,
    pub network_metered: bool,
}

/// One folder as the scheduler sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderInput {
    /// Stable folder id (matches [`Focus`] fields and the returned
    /// [`Decision::Next::folder`]).
    pub id: String,
    pub role: FolderRole,
    /// When this folder last synced *successfully*. Drives the normal
    /// role/focus/power-based interval (see [`next_sync`]). `None` means
    /// "never successfully synced" — treated as due immediately on that
    /// axis, so a brand-new folder always gets its first pass promptly
    /// rather than waiting out a full interval it never actually earned.
    pub last_synced_at: Option<i64>,
    /// When this folder was last *attempted*, successfully or not. Drives
    /// the error-backoff threshold, independently of `last_synced_at`.
    ///
    /// This field exists — separately from `last_synced_at` — because a
    /// run of failures must still move *some* clock forward on every
    /// attempt, or backoff has nothing to measure from and never actually
    /// delays anything. A folder that keeps failing never updates
    /// `last_synced_at` (there is no success to record); if backoff were
    /// keyed off `last_synced_at` instead, `consecutive_failures` would
    /// climb while the folder's due time stayed frozen at whatever it was
    /// after the last real success — i.e. the folder would look "overdue"
    /// by an ever-growing margin and get selected immediately, over and
    /// over, exactly the reconnect-storm and starvation this scheduler is
    /// required not to produce. Keying backoff off `last_attempt_at`
    /// instead means every attempt (success or failure) pushes the next
    /// eligible instant into the future, which is what makes the
    /// no-starvation argument in [`next_sync`] actually true.
    /// `None` means "never attempted".
    pub last_attempt_at: Option<i64>,
    /// Consecutive sync failures since the last success, for backoff.
    /// `0` means the last attempt (if any) succeeded, which disables the
    /// backoff threshold entirely (see [`next_sync`]).
    pub consecutive_failures: u32,
}

/// What [`next_sync`] decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Sync `folder` in `in_secs` seconds from the `now` passed in
    /// (`in_secs == 0` means "due right now"). When nothing is currently
    /// due, this still names the folder expected to come due soonest under
    /// today's inputs — a best-effort hint for how long the caller may
    /// sleep before re-evaluating, not a promise, since focus, power state,
    /// or a new failed attempt may change things before then.
    Next { folder: String, in_secs: i64 },
    /// No folders were given at all — nothing to schedule.
    Nothing,
}

/// Base interval for the folder currently open in the UI, or the folder an
/// open thread lives in. Shortest of all the bases: "the user is looking
/// at this right now" is the strongest freshness signal available, so
/// staleness here is the most visible kind.
pub const FOCUSED_INTERVAL_SECS: i64 = 20;

/// Base interval for the Inbox when it is *not* the focused folder. Still
/// short — shorter than every non-focused role below — because new mail
/// notifications originate from Inbox even while the user reads something
/// else (ТЗ §74).
pub const INBOX_INTERVAL_SECS: i64 = 45;

/// Base interval for ordinary, non-focused roles (Sent, Drafts, Archive,
/// Other). Background sync of these exists mostly to catch cross-device
/// changes, not to surface anything the user is actively waiting on.
pub const NORMAL_INTERVAL_SECS: i64 = 300;

/// Base interval for non-focused Trash/Spam: the lowest priority this
/// scheduler assigns. Still finite — see [`next_sync`]'s starvation bound —
/// just coarse, since nothing user-actionable is expected to land there
/// unprompted.
pub const LOW_INTERVAL_SECS: i64 = 900;

/// Hard ceiling on the power-adjusted normal interval, whatever combination
/// of power conditions produced it. Without this, a backgrounded,
/// on-battery, metered, idle-screen low-priority folder could compound its
/// way to an interval of hours; capping it means "eventually" on that axis
/// is always bounded, in the worst case, to 30 minutes. (The backoff axis
/// has its own, separate cap — see [`backoff_floor_secs`] — and is not
/// bounded by this constant, though in practice its own 15-minute cap is
/// well inside it.)
pub const MAX_INTERVAL_SECS: i64 = 30 * 60;

const BATTERY_MULTIPLIER: i64 = 2;
const IDLE_MULTIPLIER: i64 = 3;
const BACKGROUNDED_MULTIPLIER: i64 = 2;
const METERED_MULTIPLIER: i64 = 2;

/// How much slower background sync should be under the given power/activity
/// conditions, expressed as a multiplier on the base interval.
///
/// Multipliers stack (multiplicatively, not additively) because each
/// condition is an independent reason to be more conservative — a device
/// that is on battery *and* idle *and* on a metered link deserves to back
/// off more than any single condition alone would justify. The overall
/// result is still bounded by [`MAX_INTERVAL_SECS`] regardless of how many
/// conditions are true, so stacking can never run away.
fn power_multiplier(power: &PowerState) -> i64 {
    let mut m = 1i64;
    if power.on_battery {
        m *= BATTERY_MULTIPLIER;
    }
    if power.screen_locked_or_idle {
        m *= IDLE_MULTIPLIER;
    }
    if power.app_backgrounded {
        m *= BACKGROUNDED_MULTIPLIER;
    }
    if power.network_metered {
        m *= METERED_MULTIPLIER;
    }
    m
}

/// Per-folder error backoff threshold, in seconds, measured from
/// [`FolderInput::last_attempt_at`] (see that field's doc comment for why
/// it must be the attempt time, not the success time).
///
/// Deliberately the same step table as `feathermail_core::retry_delay_secs`
/// (`crates/core/src/queue.rs`, D32) — 2s, 5s, 15s, 30s, 60s, then doubling,
/// capped at 15 minutes — so the two backoff policies in this codebase
/// agree on "how sorry should we be after N failures" even though this
/// crate cannot import `core`'s function (no cross-crate dependency is
/// allowed here; see the crate `Cargo.toml`). Applied in [`next_sync`] as a
/// second, independent threshold — not folded into (or maxed against) the
/// normal power-adjusted interval — because mixing the two axes into one
/// number is exactly what let backoff silently stop mattering before: if
/// the "floor" is measured against the wrong timestamp, a large backoff
/// value can sit there unused. Keeping backoff as its own gate, checked
/// against its own timestamp, is what makes it actually delay anything.
fn backoff_floor_secs(consecutive_failures: u32) -> i64 {
    crate::backoff::backoff_delay_secs(consecutive_failures)
}

/// Priority tier: higher always beats lower, regardless of how overdue the
/// lower tier's folder is. This is the mechanism behind "Sent never
/// overtakes Inbox" — priority is categorical, not a function of how long
/// something has waited.
fn tier(role: FolderRole, is_focused: bool) -> u8 {
    if is_focused {
        3
    } else {
        match role {
            FolderRole::Inbox => 2,
            FolderRole::Sent | FolderRole::Drafts | FolderRole::Archive | FolderRole::Other => 1,
            FolderRole::Trash | FolderRole::Spam => 0,
        }
    }
}

fn base_interval_secs(role: FolderRole, is_focused: bool) -> i64 {
    if is_focused {
        return FOCUSED_INTERVAL_SECS;
    }
    match role {
        FolderRole::Inbox => INBOX_INTERVAL_SECS,
        FolderRole::Sent | FolderRole::Drafts | FolderRole::Archive | FolderRole::Other => {
            NORMAL_INTERVAL_SECS
        }
        FolderRole::Trash | FolderRole::Spam => LOW_INTERVAL_SECS,
    }
}

/// The role/focus/power-adjusted interval, on its own — the axis measured
/// from [`FolderInput::last_synced_at`]. Backoff is a wholly separate axis;
/// see [`due_at`] for how the two combine.
fn power_adjusted_interval_secs(role: FolderRole, is_focused: bool, power: &PowerState) -> i64 {
    let base = base_interval_secs(role, is_focused);
    base.saturating_mul(power_multiplier(power))
        .min(MAX_INTERVAL_SECS)
}

fn is_focused(id: &str, focus: &Focus<'_>) -> bool {
    focus.open_folder == Some(id) || focus.open_thread_folder == Some(id)
}

/// The instant this folder becomes eligible: the later of two independent
/// thresholds.
///
/// - The **normal** threshold, `last_synced_at + power_adjusted_interval`
///   (or `now` if never synced): "enough time has passed since the last
///   successful sync".
/// - The **backoff** threshold, `last_attempt_at + backoff_floor_secs(n)`,
///   only in effect when `consecutive_failures > 0` (otherwise it imposes
///   no constraint at all): "enough time has passed since we last tried
///   and failed".
///
/// A folder is due only once *both* have passed — i.e. `due_at` is the
/// `max` of the two, not a blend or a floor applied to one interval. This
/// is what lets a failing folder actually be delayed: its backoff
/// threshold advances every time it is *attempted*, independent of whether
/// `last_synced_at` ever moves again.
fn due_at(f: &FolderInput, is_focused: bool, power: &PowerState, now: i64) -> i64 {
    let interval = power_adjusted_interval_secs(f.role, is_focused, power);
    let normal_due_at = f.last_synced_at.map_or(now, |t| t + interval);
    let backoff_due_at = if f.consecutive_failures == 0 {
        i64::MIN // No active failures: this axis imposes no constraint.
    } else {
        let floor = backoff_floor_secs(f.consecutive_failures);
        f.last_attempt_at.map_or(now, |t| t + floor)
    };
    normal_due_at.max(backoff_due_at)
}

/// Decide which folder to sync next.
///
/// ## Algorithm
///
/// For each folder, compute its `due_at` (see [`due_at`]) and its tier
/// (focused > Inbox > ordinary > Trash/Spam). Among folders with
/// `due_at <= now` ("due"), pick the highest tier; break ties by the most
/// overdue (waited longest), then by folder id for a fully deterministic
/// result. If no folder is due yet, report the one with the earliest
/// `due_at` and how long until then.
///
/// ## Why this can't starve a low-priority folder — and what it assumes
///
/// This function is stateless: it has no memory of past calls, so the
/// no-starvation property is a claim about how a *caller* uses it, not
/// something the function enforces by itself. The required usage is: call
/// this repeatedly for the *same* `now`, and after every
/// `Decision::Next { in_secs: 0, folder }`, update that folder's input
/// before calling again —
/// - on success, set `last_synced_at = now` (and reset
///   `consecutive_failures` to `0`);
/// - on failure, set `last_attempt_at = now` and increment
///   `consecutive_failures`.
///
/// this is exactly how a driver loop should drain a "wave" of
/// simultaneously-due folders before advancing its clock. Given that the
/// caller does this, the picked folder's `due_at` strictly increases past
/// `now` on *either* outcome: on success because every interval in this
/// module is a positive number of seconds, and on failure because
/// `backoff_floor_secs` is at least `2` for any nonzero failure count. So
/// either way the folder immediately stops being a candidate for this same
/// `now`. That means the set of due folders strictly shrinks by one on
/// every call regardless of whether the sync attempt succeeds, so a folder
/// due at time `now` is guaranteed to be picked within at most
/// `folders.len()` calls at that `now` — it can be blocked by every
/// *other* due folder at most once each, never repeatedly by the same
/// higher-priority one, whether or not that folder's own attempts are
/// succeeding. No aging counters or randomness are needed to get this
/// guarantee; it falls out of tiers being categorical and both axes'
/// intervals being strictly positive on every attempt. What this function
/// cannot prevent on its own: a caller that repeatedly asks about a failing
/// folder without ever recording the attempt (i.e. without bumping
/// `last_attempt_at`/`consecutive_failures`) will keep seeing `in_secs: 0`
/// for it forever, exactly like asking the same question twice and
/// expecting a different answer — the backoff clock only advances because
/// the caller told it an attempt happened.
pub fn next_sync(
    folders: &[FolderInput],
    focus: &Focus<'_>,
    power: &PowerState,
    now: i64,
) -> Decision {
    if folders.is_empty() {
        return Decision::Nothing;
    }

    struct Computed<'a> {
        id: &'a str,
        tier: u8,
        due_at: i64,
    }

    let computed: Vec<Computed<'_>> = folders
        .iter()
        .map(|f| {
            let focused = is_focused(&f.id, focus);
            Computed {
                id: &f.id,
                tier: tier(f.role, focused),
                due_at: due_at(f, focused, power, now),
            }
        })
        .collect();

    let due_now: Vec<&Computed<'_>> = computed.iter().filter(|c| c.due_at <= now).collect();

    if let Some(winner) = due_now.into_iter().max_by(|a, b| {
        a.tier
            .cmp(&b.tier)
            // More overdue (smaller/earlier due_at) wins within a tier.
            .then_with(|| b.due_at.cmp(&a.due_at))
            // Deterministic fallback: no meaning beyond stable ordering.
            .then_with(|| b.id.cmp(a.id))
    }) {
        return Decision::Next {
            folder: winner.id.to_string(),
            in_secs: 0,
        };
    }

    // Nothing due yet: report whichever folder becomes due soonest.
    let soonest = computed
        .iter()
        .min_by(|a, b| a.due_at.cmp(&b.due_at).then_with(|| a.id.cmp(b.id)))
        .expect("folders is non-empty");
    Decision::Next {
        folder: soonest.id.to_string(),
        in_secs: soonest.due_at - now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(id: &str, role: FolderRole, last_synced_at: Option<i64>) -> FolderInput {
        FolderInput {
            id: id.to_string(),
            role,
            last_synced_at,
            last_attempt_at: last_synced_at,
            consecutive_failures: 0,
        }
    }

    fn active_power() -> PowerState {
        PowerState::default()
    }

    #[test]
    fn inbox_and_sent_baselines_and_tiers_differ() {
        assert!(tier(FolderRole::Inbox, false) > tier(FolderRole::Sent, false));
    }

    /// The check from docs/plan.md T-023, verbatim: "симуляция «пользователь
    /// в Inbox» — Sent не обгоняет Inbox". Sent has been waiting far longer
    /// (hugely overdue) than Inbox (just barely due) — a naive "most
    /// overdue wins" policy would pick Sent. It must still lose to Inbox.
    #[test]
    fn user_in_inbox_sent_never_overtakes_inbox() {
        let now = 1_000_000i64;
        let folders = vec![
            // Exactly due, no slack at all.
            folder("inbox", FolderRole::Inbox, Some(now - INBOX_INTERVAL_SECS)),
            // Absurdly overdue by comparison.
            folder("sent", FolderRole::Sent, Some(now - 1_000_000)),
        ];
        let focus = Focus {
            open_folder: Some("inbox"),
            open_thread_folder: None,
        };
        let decision = next_sync(&folders, &focus, &active_power(), now);
        assert_eq!(
            decision,
            Decision::Next {
                folder: "inbox".to_string(),
                in_secs: 0
            },
            "Inbox's tier must win even though Sent is vastly more overdue"
        );
    }

    /// Extend the single-instant check into a running simulation: keep the
    /// user parked in Inbox and drain wave after wave. Whenever Sent is
    /// actually picked, Inbox must not have been simultaneously due at that
    /// same moment — i.e. Sent is never chosen "instead of" a due Inbox.
    #[test]
    fn user_in_inbox_simulation_sent_never_overtakes_inbox_over_time() {
        let mut inbox_last = 0i64;
        let mut sent_last = 0i64;
        let mut now = 0i64;
        let focus = Focus {
            open_folder: Some("inbox"),
            open_thread_folder: None,
        };
        let power = active_power();

        for _ in 0..500 {
            let folders = vec![
                folder("inbox", FolderRole::Inbox, Some(inbox_last)),
                folder("sent", FolderRole::Sent, Some(sent_last)),
            ];
            match next_sync(&folders, &focus, &power, now) {
                Decision::Next { folder, in_secs: 0 } => {
                    let inbox_due_at = inbox_last + INBOX_INTERVAL_SECS;
                    if folder == "sent" {
                        assert!(
                            inbox_due_at > now,
                            "Sent was picked at t={now} while Inbox was also due (due_at={inbox_due_at})"
                        );
                        sent_last = now;
                    } else {
                        assert_eq!(folder, "inbox");
                        inbox_last = now;
                    }
                }
                Decision::Next { in_secs, .. } => {
                    assert!(in_secs > 0);
                    now += in_secs;
                }
                Decision::Nothing => unreachable!("folders is non-empty"),
            }
        }
    }

    #[test]
    fn battery_and_idle_increase_intervals_with_concrete_numbers() {
        let now = 0i64;
        let synced_now = vec![folder("inbox", FolderRole::Inbox, Some(now))];
        let focus = Focus::default();

        let active = next_sync(&synced_now, &focus, &active_power(), now);
        assert_eq!(
            active,
            Decision::Next {
                folder: "inbox".to_string(),
                in_secs: INBOX_INTERVAL_SECS
            }
        );
        assert_eq!(
            active,
            Decision::Next {
                folder: "inbox".to_string(),
                in_secs: 45
            }
        );

        let sluggish = PowerState {
            on_battery: true,
            screen_locked_or_idle: true,
            app_backgrounded: false,
            network_metered: false,
        };
        let idle = next_sync(&synced_now, &focus, &sluggish, now);
        // 45 * 2 (battery) * 3 (idle) = 270.
        assert_eq!(
            idle,
            Decision::Next {
                folder: "inbox".to_string(),
                in_secs: 270
            }
        );

        let Decision::Next {
            in_secs: active_secs,
            ..
        } = active
        else {
            unreachable!()
        };
        let Decision::Next {
            in_secs: idle_secs, ..
        } = idle
        else {
            unreachable!()
        };
        assert!(
            idle_secs > active_secs,
            "battery+idle ({idle_secs}s) must be strictly slower than active ({active_secs}s)"
        );
    }

    #[test]
    fn all_four_power_conditions_stack_but_stay_capped() {
        let now = 0i64;
        let folders = vec![folder("trash", FolderRole::Trash, Some(now))];
        let worst = PowerState {
            on_battery: true,
            screen_locked_or_idle: true,
            app_backgrounded: true,
            network_metered: true,
        };
        // 900 * 2 * 3 * 2 * 2 = 21_600, but capped at MAX_INTERVAL_SECS.
        let decision = next_sync(&folders, &Focus::default(), &worst, now);
        assert_eq!(
            decision,
            Decision::Next {
                folder: "trash".to_string(),
                in_secs: MAX_INTERVAL_SECS
            }
        );
        assert_eq!(MAX_INTERVAL_SECS, 1800);
    }

    /// No-starvation guarantee: with N folders all due at once, draining
    /// same-`now` waves (as documented on `next_sync`, and *assuming* the
    /// caller records every pick — success or failure — before asking
    /// again) must surface the lowest-tier folder within at most N calls —
    /// here N = 4, so Trash must appear by the 4th call at the latest. To
    /// actually exercise the "success or failure" half of that assumption
    /// (not just the success half), the Inbox pick in this wave is recorded
    /// as a *failed* attempt rather than a successful sync — proving a
    /// failing high-tier folder still frees its slot for the wave to
    /// continue, rather than being retried in place.
    #[test]
    fn low_priority_folder_is_synced_within_bounded_steps_even_with_a_failing_pick() {
        let now = 1_000i64;
        let long_ago = now - 100_000;
        let mut inputs = vec![
            folder("inbox", FolderRole::Inbox, Some(long_ago)),
            folder("sent", FolderRole::Sent, Some(long_ago)),
            folder("archive", FolderRole::Archive, Some(long_ago)),
            folder("trash", FolderRole::Trash, Some(long_ago)),
        ];
        let focus = Focus::default();
        let power = active_power();
        let bound = inputs.len();

        let mut order = Vec::new();
        for _ in 0..bound {
            let decision = next_sync(&inputs, &focus, &power, now);
            let Decision::Next { folder, in_secs } = decision else {
                panic!("expected a due folder every call in this wave");
            };
            assert_eq!(
                in_secs, 0,
                "every call in the wave should find something due"
            );
            let picked = inputs.iter_mut().find(|f| f.id == folder).unwrap();
            if picked.id == "inbox" {
                // Record a failed attempt, not a success: last_synced_at is
                // deliberately left untouched.
                picked.last_attempt_at = Some(now);
                picked.consecutive_failures += 1;
            } else {
                picked.last_synced_at = Some(now);
                picked.last_attempt_at = Some(now);
            }
            order.push(folder);
        }

        assert_eq!(order.len(), 4);
        assert!(
            order.contains(&"trash".to_string()),
            "trash must have been synced within {bound} calls, order was {order:?}"
        );
        assert_eq!(
            order.last(),
            Some(&"trash".to_string()),
            "trash is lowest-tier, so it must be last in a wave where everything started due"
        );
        assert_eq!(
            order[0], "inbox",
            "inbox is highest tier and must still be picked first even though its attempt will fail"
        );

        // The wave is now fully drained: a 5th call must find nothing due,
        // including for the just-failed inbox (its backoff has kicked in).
        let after = next_sync(&inputs, &focus, &power, now);
        match after {
            Decision::Next { in_secs, .. } => assert!(in_secs > 0),
            Decision::Nothing => panic!("folders is non-empty"),
        }
    }

    #[test]
    fn backoff_floor_matches_core_shape_and_is_capped() {
        assert_eq!(backoff_floor_secs(0), 0);
        assert_eq!(backoff_floor_secs(1), 2);
        assert_eq!(backoff_floor_secs(2), 5);
        assert_eq!(backoff_floor_secs(3), 15);
        assert_eq!(backoff_floor_secs(4), 30);
        assert_eq!(backoff_floor_secs(5), 60);
        assert_eq!(backoff_floor_secs(6), 120);
        assert_eq!(backoff_floor_secs(7), 240);
        assert_eq!(backoff_floor_secs(8), 480);
        assert_eq!(backoff_floor_secs(9), 900);
        assert_eq!(
            backoff_floor_secs(50),
            900,
            "must not exceed the 15-minute cap"
        );
    }

    #[test]
    fn repeated_failures_never_exceed_the_overall_ceiling() {
        let now = 0i64;
        // Trash's own base (900s) already needs the worst-case power stack
        // to threaten the ceiling; piling on a huge failure count as well
        // must still not push the result past MAX_INTERVAL_SECS.
        let folders = vec![FolderInput {
            id: "trash".to_string(),
            role: FolderRole::Trash,
            last_synced_at: Some(now),
            last_attempt_at: Some(now),
            consecutive_failures: 10_000,
        }];
        let worst = PowerState {
            on_battery: true,
            screen_locked_or_idle: true,
            app_backgrounded: true,
            network_metered: true,
        };
        let decision = next_sync(&folders, &Focus::default(), &worst, now);
        assert_eq!(
            decision,
            Decision::Next {
                folder: "trash".to_string(),
                in_secs: MAX_INTERVAL_SECS
            },
            "no reconnect storm: even massive failure counts plus worst-case power stay at the overall cap"
        );

        // And on its own (no power penalty at all), backoff never exceeds
        // its documented 15-minute floor even at this failure count.
        assert_eq!(backoff_floor_secs(10_000), 15 * 60);
    }

    #[test]
    fn never_synced_folder_is_due_immediately() {
        let now = 500i64;
        let folders = vec![folder("inbox", FolderRole::Inbox, None)];
        let decision = next_sync(&folders, &Focus::default(), &active_power(), now);
        assert_eq!(
            decision,
            Decision::Next {
                folder: "inbox".to_string(),
                in_secs: 0
            }
        );
    }

    #[test]
    fn focused_non_inbox_folder_beats_inbox() {
        let now = 1_000i64;
        let folders = vec![
            folder("inbox", FolderRole::Inbox, Some(now - INBOX_INTERVAL_SECS)),
            folder(
                "archive",
                FolderRole::Archive,
                Some(now - FOCUSED_INTERVAL_SECS),
            ),
        ];
        let focus = Focus {
            open_folder: Some("archive"),
            open_thread_folder: None,
        };
        let decision = next_sync(&folders, &focus, &active_power(), now);
        assert_eq!(
            decision,
            Decision::Next {
                folder: "archive".to_string(),
                in_secs: 0
            },
            "the folder the user is actually looking at outranks Inbox itself"
        );
    }

    #[test]
    fn open_thread_folder_counts_as_focus_even_if_a_different_folder_is_displayed() {
        let now = 1_000i64;
        let folders = vec![
            folder("inbox", FolderRole::Inbox, Some(now - INBOX_INTERVAL_SECS)),
            folder(
                "archive",
                FolderRole::Archive,
                Some(now - FOCUSED_INTERVAL_SECS),
            ),
        ];
        // e.g. a thread opened from a cross-folder search results view.
        let focus = Focus {
            open_folder: Some("search-results"),
            open_thread_folder: Some("archive"),
        };
        let decision = next_sync(&folders, &focus, &active_power(), now);
        assert_eq!(
            decision,
            Decision::Next {
                folder: "archive".to_string(),
                in_secs: 0
            }
        );
    }

    #[test]
    fn empty_folder_set_yields_nothing() {
        let decision = next_sync(&[], &Focus::default(), &active_power(), 0);
        assert_eq!(decision, Decision::Nothing);
    }

    /// The bug this test guards against: backoff was previously measured
    /// from `last_synced_at` (last *success*), so a folder that succeeded
    /// long ago and has since failed repeatedly looked permanently overdue
    /// by the normal metric and was returned with `in_secs: 0` forever,
    /// no matter how large `consecutive_failures` grew. This is the exact
    /// scenario from review: succeeded an hour ago, 10 failures since, most
    /// recent attempt just now.
    #[test]
    fn failing_folder_is_delayed_by_backoff_not_returned_immediately() {
        let now = 10_000i64;
        let folders = vec![FolderInput {
            id: "inbox".to_string(),
            role: FolderRole::Inbox,
            last_synced_at: Some(now - 3_600), // succeeded an hour ago
            last_attempt_at: Some(now),        // just failed again
            consecutive_failures: 10,
        }];
        let decision = next_sync(&folders, &Focus::default(), &active_power(), now);
        assert_eq!(
            decision,
            Decision::Next {
                folder: "inbox".to_string(),
                in_secs: backoff_floor_secs(10)
            },
            "backoff must delay the next attempt, not be overridden by how overdue \
             last_synced_at makes the folder look"
        );
        assert_eq!(backoff_floor_secs(10), 900);
    }

    /// Ten consecutive calls, recording a failure after each pick and never
    /// a success in between, must not hand back ten immediate
    /// (`in_secs == 0`) decisions for the same folder — that would be
    /// exactly the reconnect storm the ticket forbids. Only the very first
    /// call (before any backoff has ever been recorded) may be immediate;
    /// backoff must make every subsequent one wait.
    #[test]
    fn ten_consecutive_failed_attempts_do_not_yield_ten_immediate_picks() {
        let now = 0i64;
        let mut input = FolderInput {
            id: "inbox".to_string(),
            role: FolderRole::Inbox,
            last_synced_at: None,
            last_attempt_at: None,
            consecutive_failures: 0,
        };
        let mut immediate_picks = 0u32;

        for _ in 0..10 {
            let folders = [input.clone()];
            match next_sync(&folders, &Focus::default(), &active_power(), now) {
                Decision::Next { in_secs: 0, .. } => {
                    immediate_picks += 1;
                    input.last_attempt_at = Some(now);
                    input.consecutive_failures += 1;
                }
                Decision::Next { in_secs, .. } => assert!(in_secs > 0),
                Decision::Nothing => unreachable!("folders is non-empty"),
            }
        }

        assert_eq!(
            immediate_picks, 1,
            "only the very first attempt (before any backoff exists) may be immediate; \
             every subsequent call, still at the same `now` and still with no success, \
             must be held back by backoff instead of firing again right away"
        );
    }

    /// A high-tier folder that is actively backing off from failures must
    /// not block a due low-tier folder from getting its turn — the two are
    /// on independent clocks now, so Inbox being "busy failing" is not the
    /// same as Inbox being due.
    #[test]
    fn trash_gets_a_turn_while_inbox_is_backing_off_from_failures() {
        let now = 10_000i64;
        let folders = vec![
            // Inbox: currently inside its backoff window (failed moments
            // ago), so it is NOT due right now despite being the higher tier.
            FolderInput {
                id: "inbox".to_string(),
                role: FolderRole::Inbox,
                last_synced_at: Some(now - 100_000),
                last_attempt_at: Some(now - 10), // 10s ago
                consecutive_failures: 5,         // backoff floor = 60s > 10s elapsed
            },
            // Trash: genuinely due, no failures at all.
            folder("trash", FolderRole::Trash, Some(now - 100_000)),
        ];
        let decision = next_sync(&folders, &Focus::default(), &active_power(), now);
        assert_eq!(
            decision,
            Decision::Next {
                folder: "trash".to_string(),
                in_secs: 0
            },
            "a failing higher-tier folder must not starve a due lower-tier one \
             while it is still inside its own backoff window"
        );
    }
}
