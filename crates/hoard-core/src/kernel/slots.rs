//! Which folder of a game is which, and the same one across machines.
//!
//! A game almost never keeps everything in one place. Factorio has the saves in
//! `Factorio/saves` and the settings in `Factorio/config`; a Paradox game splits
//! saves and mods; an emulator separates memory cards from BIOS files. Tracking
//! a single folder per title forces a choice, and making that choice by hand —
//! pointing at the second folder — is what, until aug-2026, left the card
//! showing only that one with the real folder nowhere in sight.
//!
//! Here a title stops having *one* folder and gets a numbered list. The number
//! is all Hoard needs to know:
//!
//! * **Slot 1 is always the saved games.** It is what detection proposes, what
//!   gets restored on its own on a fresh machine, and what counts as "this game
//!   is synced".
//! * **From 2 up it is everything else**, and Hoard does not try to guess what.
//!   It is backed up all the same, but nothing is written over it without
//!   someone pressing the button: one machine's config folder has no business
//!   landing on another's.
//!
//! ## Why a number and not the folder's name
//!
//! The number is the **identity across machines**, so it has to be something
//! both of them can work out without talking to each other. The path won't do:
//! Factorio's config lives in `%APPDATA%\Factorio\config` on Windows and in
//! `~/.factorio/config` on Linux, so pairing by path pairs nothing. Neither
//! will a name — that needs someone to type the same thing twice.
//!
//! With a number, machine B can see the title has a slot 2 in the cloud that it
//! doesn't have locally, and say "this folder here is my 2" with one click.
//! Hoard never decides what goes with what; it just carries whatever is in each
//! number.
//!
//! And the number has to be **the user's to pick**, not assigned in arrival
//! order. Auto-numbering was tried first and fails at exactly the job the slots
//! exist for: the same folder added on two machines came out as 2 on Windows and
//! 3 on Linux — because by then Linux could already see Windows' 2 taken — so
//! the two never paired up.
//!
//! ## How it is stored
//!
//! In the save's `label`, which already exists and is already covered by
//! `UNIQUE(user_id, game_slug, label)` server-side — that is, the "one slot per
//! number and title" constraint was already written, no migration needed.
//!
//! Slot 1 is written `"main"`, not `"1"`, because `"main"` is what every save
//! tracked so far already carries in the cloud, and renaming those would move
//! their history for no reason at all. The asymmetry is ugly and cheap: both
//! machines compute the same `label` for the same slot, which is the only thing
//! that has to hold for them to recognise each other.

/// The saved-games slot: what detection proposes, what restores on its own, and
/// what decides whether a game counts as synced.
pub const SAVES: u32 = 1;

/// Historical labels that mean slot 1. `"main"` is what the client has always
/// written; `"default"` is what the server fills in when an upload arrives
/// without one (see the `unwrap_or_else` in `/v1/snapshots`).
const LEGACY_SAVES_LABELS: [&str; 2] = ["main", "default"];

/// The `label` a slot is stored under. See the module docs for why slot 1 is
/// `"main"`.
pub fn label_for(slot: u32) -> String {
    if slot == SAVES {
        LEGACY_SAVES_LABELS[0].to_string()
    } else {
        slot.to_string()
    }
}

/// Which slot a `label` is, or `None` for one of the older free-form labels.
///
/// `None` is not an error: the label used to be whatever text the user wanted
/// (and the "track this path" button went as far as stuffing the whole path in
/// there). Those rows still exist, still sync, and still render with their own
/// text; they just have no number until someone gives them one.
pub fn slot_of(label: &str) -> Option<u32> {
    let t = label.trim();
    if LEGACY_SAVES_LABELS.iter().any(|l| t.eq_ignore_ascii_case(l)) {
        return Some(SAVES);
    }
    // Digits only, and it has to fit: `"01"` and `"1"` are the same slot, but
    // `"2 config"` is not a number and `"99999999999"` is not a slot.
    if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    t.parse::<u32>().ok().filter(|n| *n >= SAVES)
}

/// The lowest free number, given the slots a title already occupies.
///
/// Lowest rather than "last + 1" so that deleting slot 2 and adding another
/// folder hands back a 2, instead of leaving a hole and counting on from 4. The
/// holes matter: the number is what the other machine sees, and a list reading
/// `1, 4, 7` tells nobody anything.
pub fn next_free(taken: impl IntoIterator<Item = u32>) -> u32 {
    let mut taken: Vec<u32> = taken.into_iter().collect();
    taken.sort_unstable();
    let mut next = SAVES;
    for n in taken {
        if n == next {
            next += 1;
        } else if n > next {
            break;
        }
    }
    next
}

/// Does this slot restore on its own?
///
/// Only slot 1. From 2 up Hoard has no idea what it holds — this machine's
/// config, mods, screenshots — and writing that over whatever another machine
/// has is exactly the damage that file classification
/// ([`crate::kernel::fileclass`]) spends its day avoiding *within* a folder. It
/// is still backed up: pulling it down is one button.
pub fn restores_automatically(slot: Option<u32>) -> bool {
    // An older free-form label counts as saved games: those rows already
    // existed, already restored on their own, and taking that away on upgrade
    // would be a silent regression.
    slot.is_none_or(|s| s == SAVES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_slot_round_trips_through_the_legacy_label() {
        assert_eq!(label_for(SAVES), "main");
        assert_eq!(slot_of("main"), Some(1));
        assert_eq!(slot_of("default"), Some(1));
        assert_eq!(slot_of("1"), Some(1));
    }

    #[test]
    fn extra_slots_are_their_own_number() {
        for n in [2u32, 3, 17] {
            assert_eq!(slot_of(&label_for(n)), Some(n), "slot {n}");
        }
    }

    /// The older free-form labels neither break nor get handed a number.
    #[test]
    fn free_labels_have_no_slot() {
        for label in [
            "ironman",
            "",
            "  ",
            "2 config",
            r"C:\Users\rl261\Desktop\saves",
            "0",
            "99999999999999999999",
        ] {
            assert_eq!(slot_of(label), None, "{label:?} is not a slot");
        }
    }

    #[test]
    fn next_free_fills_the_lowest_gap() {
        assert_eq!(next_free([]), 1);
        assert_eq!(next_free([1]), 2);
        assert_eq!(next_free([1, 2, 3]), 4);
        assert_eq!(next_free([1, 3]), 2, "fills the hole before growing");
        assert_eq!(next_free([2, 3]), 1);
        assert_eq!(next_free([3, 1, 2]), 4, "input order is irrelevant");
    }

    #[test]
    fn only_the_saves_slot_restores_on_its_own() {
        assert!(restores_automatically(Some(SAVES)));
        assert!(!restores_automatically(Some(2)));
        assert!(
            restores_automatically(None),
            "older free-form labels already restored on their own"
        );
    }
}
