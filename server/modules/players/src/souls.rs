use std::collections::BTreeMap;
use std::sync::OnceLock;

use resonantdust_content::definition_core::find_packed_by_key;
use spacetimedb::{table, ReducerContext, Table};

use crate::cards;
use crate::cards::Card;
use crate::flags::state_flags;
use crate::packed::{pack_valid_at, valid_at_time};
use crate::sequence;

/// Read the soul portrait index (0..=15) out of a `Card.flags_state`
/// value. Meaningful only on soul cards; non-soul callers should
/// ignore the result.
pub fn portrait_id(flags_state: u32) -> u8 {
    let s = state_flags();
    ((flags_state & s.portrait_id_mask) >> s.portrait_id_shift) as u8
}

/// Stamp the soul portrait index into a `Card.flags_state` value,
/// clearing any prior value first. `id` is masked to 4 bits, so
/// callers don't have to range-check; passing `id >= 16` silently
/// truncates to the low nibble.
pub fn with_portrait(flags_state: u32, id: u8) -> u32 {
    let s = state_flags();
    (flags_state & !s.portrait_id_mask) | (((id as u32) & 0xF) << s.portrait_id_shift)
}

/// Soul row — one per soul card, public so clients can subscribe.
///
/// Positional fields (`macro_zone`, `micro_location`) mirror the soul
/// card itself; they're kept in sync by the `on_card_write` hook that
/// fires from `cards::write_at` on every card insert. Souls are loose on
/// the world surface, so `micro_location` carries packed loose coords
/// (`micro_is_card` is clear). Stats / fatigued / injured fields each pack four
/// `u8` counters into a `u32` (byte 0 = corpus, 1 = anima,
/// 2 = sollertia, 3 = aether — same order across all three fields):
///
/// - `stats`    — counts of `corpus` / `anima` / `sollertia` / `aether`
///   faculty cards currently owned by this soul's player.
/// - `fatigued` — counts of the `-` variants
///   (`corpus-` / `anima-` / `sollertia-` / `aether-`).
/// - `injured`  — counts of the `-i` variants (reserved; mapping is
///   empty until the injured-faculty cards are added to content).
///
/// Counts saturate at `u8::MAX` (255) — a soul accumulating more than
/// that many of a single faculty is well outside design space, but the
/// `saturating_add` keeps the diff hook honest in the pathological
/// case.
#[table(accessor = souls, public)]
#[derive(Debug, Clone)]
pub struct Soul {
    /// Packed primary key — `[time_ms: u48 | seq: u16]`. Same
    /// history-row pattern `cards` / `players` / `zones` use. The
    /// `card_id` is on the row column (see below); the row with the
    /// largest `valid_at_time` is the active state.
    #[primary_key]
    pub valid_at: u64,
    /// Data-shard partition this row belongs to (`crate::DATA_SHARD`; `0` today).
    pub data_shard: u16,
    #[index(btree)]
    pub card_id: u32,
    #[index(btree)]
    pub owner_id: u32,
    pub macro_zone: u64,
    pub micro_location: u32,
    /// `[corpus | anima | sollertia | aether]` packed little-endian.
    pub stats: u32,
    /// `[corpus- | anima- | sollertia- | aether-]` packed little-endian.
    pub fatigued: u32,
    /// `[corpus-i | anima-i | sollertia-i | aether-i]` packed
    /// little-endian. Counters stay 0 until the `-i` faculty cards
    /// exist in content and `stat_slot_for` maps them in.
    pub injured: u32,
}

/// Per-soul private state — the stuff the owning soul needs but
/// other players don't (discovered blueprints, etc.). Kept off the
/// public `Soul` row so other clients mirroring souls visible in
/// their loaded zone don't pull in this soul's personal progression.
///
/// **Subscription pattern.** Public table, but each client only
/// subscribes to their own active soul's row via
/// `WHERE card_id = <local soul's card_id>`. Same convention as
/// [`crate::players::PlayerProfile`] — server can't enforce "no
/// peeking at others" today, so this is fine for low-sensitivity
/// progression bits but not for anything truly sensitive.
///
/// **Flat row, not history.** Unlike `Soul` / `Card`, this table has
/// one row per `card_id` and is updated in place. Progression state
/// isn't time-stamped — there are no "what blueprints did the soul
/// have discovered at time T" reads downstream — so the `valid_at`
/// history machinery would be deadweight.
///
/// **Initial row.** Created in `players::spawn_soul_for` right after
/// the soul card itself, alongside its starter inventory. Seeded with
/// the soul's starter blueprints; further discovery is gameplay-driven.
#[table(accessor = soul_privates, public)]
#[derive(Debug, Clone)]
pub struct SoulPrivate {
    #[primary_key]
    pub card_id: u32,
    /// Data-shard partition this row belongs to (`crate::DATA_SHARD`; `0` today).
    pub data_shard: u16,
    /// Bit field of discovered blueprints, ids 1..=64. Bit position
    /// is `blueprint_id - 1`, matching the 1-indexed id mapping in
    /// `content/blueprints/id.json` (so blueprint id 1 = bit 0).
    /// `blueprints_0` covers the first 64 ids; further fields
    /// (`blueprints_1`, …) will be appended as the catalog grows
    /// past 64 entries. Default `0` — discovery is gameplay-driven,
    /// nothing is granted on signup. Flipping a bit on is one-way.
    pub blueprints_0: u64,
    /// Count of live blueprint cards owned by this soul ("under
    /// construction" slots). Maintained by `on_card_write` the
    /// same way `Soul.stats` is maintained: a blueprint card
    /// coming alive bumps +1 on the owning soul's
    /// `active_blueprints`; going dead / changing owner bumps -1.
    /// `blueprints::request_blueprint` reads this against the
    /// soul's `builder` aspect value to decide whether to allow a
    /// new request — no separate slot-release reducer needed,
    /// because the hook handles release implicitly when the
    /// blueprint dies. Saturates at `u8::MAX`.
    pub active_blueprints: u8,
}

// ---- u32 quad packing -----------------------------------------------

/// Pack four `u8`s into a `u32` in the same order they're laid out in
/// `stats` / `fatigued` / `injured`. Little-endian: byte `i` occupies
/// bits `i*8 .. i*8 + 8`.
pub fn pack_quad(b0: u8, b1: u8, b2: u8, b3: u8) -> u32 {
    (b0 as u32) | ((b1 as u32) << 8) | ((b2 as u32) << 16) | ((b3 as u32) << 24)
}

/// Inverse of [`pack_quad`].
pub fn unpack_quad(v: u32) -> (u8, u8, u8, u8) {
    (
        v as u8,
        (v >> 8) as u8,
        (v >> 16) as u8,
        (v >> 24) as u8,
    )
}

/// Read one byte (`byte_index` 0..=3) out of a packed quad.
pub fn quad_get(v: u32, byte_index: u8) -> u8 {
    (v >> (byte_index as u32 * 8)) as u8
}

/// Replace one byte (`byte_index` 0..=3) inside a packed quad.
pub fn quad_set(v: u32, byte_index: u8, byte: u8) -> u32 {
    let shift = byte_index as u32 * 8;
    let mask = !(0xFF_u32 << shift);
    (v & mask) | ((byte as u32) << shift)
}

// ---- CRUD ----------------------------------------------------------

fn now_ms(ctx: &ReducerContext) -> u64 {
    (ctx.timestamp.to_micros_since_unix_epoch() / 1_000) as u64
}

pub fn latest(ctx: &ReducerContext, card_id: u32) -> Option<Soul> {
    ctx.db
        .souls()
        .card_id()
        .filter(card_id)
        .max_by_key(|s| valid_at_time(s.valid_at))
}

fn write(ctx: &ReducerContext, soul: Soul) -> Soul {
    write_at(ctx, soul, now_ms(ctx))
}

fn write_at(ctx: &ReducerContext, mut soul: Soul, time_ms: u64) -> Soul {
    // "Last write at this (card_id, time_ms) wins." The new sequence-
    // bearing PK is unique per write, so without this purge an
    // in-reducer accumulator like `apply_slot_delta` firing 3 times
    // at `now_ms` (bootstrap's 3-corpus path) would leave 3 distinct
    // soul rows at the same time, each with a different stats value,
    // and `latest()` would pick whichever max_by_key returns on ties
    // — non-deterministic and almost always wrong.
    let stale: Vec<u64> = ctx
        .db
        .souls()
        .card_id()
        .filter(soul.card_id)
        .filter(|s| valid_at_time(s.valid_at) == time_ms)
        .map(|s| s.valid_at)
        .collect();
    for v in stale {
        ctx.db.souls().valid_at().delete(v);
    }
    soul.valid_at = pack_valid_at(time_ms, sequence::next_sequence(ctx));
    let inserted = ctx.db.souls().insert(soul);
    // No per-write delete schedule — `crate::gc` handles
    // prior-version reap on its periodic sweep.
    inserted
}

#[allow(dead_code)]
pub fn update_with<F>(ctx: &ReducerContext, card_id: u32, f: F) -> Option<Soul>
where
    F: FnOnce(&mut Soul),
{
    let mut s = latest(ctx, card_id)?;
    f(&mut s);
    Some(write(ctx, s))
}

pub fn update_with_at<F>(
    ctx: &ReducerContext,
    card_id: u32,
    time_ms: u64,
    f: F,
) -> Option<Soul>
where
    F: FnOnce(&mut Soul),
{
    let mut s = latest(ctx, card_id)?;
    f(&mut s);
    Some(write_at(ctx, s, time_ms))
}

// ---- stat-slot mapping ---------------------------------------------

/// Which packed `u32` field on `Soul` a counter lives in. The byte
/// index inside that field is carried separately on [`StatSlot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatField {
    Stats,
    Fatigued,
    #[allow(dead_code)] // wired into the table but no card defs map here yet
    Injured,
}

/// Address of a single stat counter inside a Soul row: the field
/// (`stats` / `fatigued` / `injured`) and the byte index within it
/// (0 = corpus, 1 = anima, 2 = sollertia, 3 = aether).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatSlot {
    pub field: StatField,
    pub byte_index: u8,
}

/// Lazy `packed_definition → StatSlot` map, built on first use by
/// resolving each tracked card key against the content registry. Keys
/// that don't resolve are silently skipped — works around the case
/// where the content catalog hasn't shipped a particular stat card
/// yet (the `-i` injured variants are the obvious example today).
fn stat_map() -> &'static BTreeMap<u16, StatSlot> {
    static STAT_MAP: OnceLock<BTreeMap<u16, StatSlot>> = OnceLock::new();
    STAT_MAP.get_or_init(|| {
        // Byte indices match the documented layout: corpus=0, anima=1,
        // sollertia=2, aether=3. Same across stats / fatigued / injured.
        let entries: &[(&str, StatField, u8)] = &[
            ("corpus",     StatField::Stats,    0),
            ("anima",      StatField::Stats,    1),
            ("sollertia",  StatField::Stats,    2),
            ("aether",     StatField::Stats,    3),
            ("corpus-",    StatField::Fatigued, 0),
            ("anima-",     StatField::Fatigued, 1),
            ("sollertia-", StatField::Fatigued, 2),
            ("aether-",    StatField::Fatigued, 3),
            ("corpus-i",   StatField::Injured,  0),
            ("anima-i",    StatField::Injured,  1),
            ("sollertia-i",StatField::Injured,  2),
            ("aether-i",   StatField::Injured,  3),
        ];
        let mut m = BTreeMap::new();
        for &(key, field, byte_index) in entries {
            if let Ok(Some(packed)) = find_packed_by_key(key) {
                m.insert(packed, StatSlot { field, byte_index });
            }
        }
        m
    })
}

/// Resolve the `Soul` stat slot a card contributes to, if any.
/// Returns `None` for non-stat cards (tiles, soul cards themselves,
/// revery, discipline, etc.).
pub fn stat_slot_for(packed_def: u16) -> Option<StatSlot> {
    stat_map().get(&packed_def).copied()
}

/// Quick-check helper: read the current value of one stat slot for
/// the soul card identified by `card_id`. Returns `None` when no Soul
/// row exists (e.g., the player has no soul yet). Useful for action
/// preconditions like "needs at least N corpus" without callers
/// having to know the packed-u32 layout.
#[allow(dead_code)]
pub fn read_slot(ctx: &ReducerContext, card_id: u32, slot: StatSlot) -> Option<u8> {
    let soul = latest(ctx, card_id)?;
    let field = match slot.field {
        StatField::Stats => soul.stats,
        StatField::Fatigued => soul.fatigued,
        StatField::Injured => soul.injured,
    };
    Some(quad_get(field, slot.byte_index))
}

// ---- soul-card identity --------------------------------------------

/// Card-type id for `soul` cards. Mirrors `cards/types.json` —
/// promote to a content-side helper if more modules need it.
const SOUL_CARD_TYPE: u8 = 6;

pub fn is_soul_card(packed_def: u16) -> bool {
    ((packed_def >> 12) & 0xF) as u8 == SOUL_CARD_TYPE
}

/// Card-type id for `blueprint` cards. Mirrors `cards/types.json`
/// (same hardcode style as `SOUL_CARD_TYPE` above; type ids in
/// `types.json` are stable, so embedding them avoids a registry
/// lookup on every card write).
const BLUEPRINT_CARD_TYPE: u8 = 1;

fn is_blueprint_card(packed_def: u16) -> bool {
    ((packed_def >> 12) & 0xF) as u8 == BLUEPRINT_CARD_TYPE
}

/// What this card row contributes to
/// `SoulPrivate.active_blueprints` — the soul that owns it (via
/// the owner chain). Returns `None` for non-blueprint cards, dead
/// cards, world-owned cards (no soul in the chain), and the
/// degenerate case where `owner_id == 0`. Mirrors the shape of
/// [`card_contribution`] for stats.
fn blueprint_contribution(ctx: &ReducerContext, card: &Card) -> Option<u32> {
    if !is_blueprint_card(card.packed_definition) {
        return None;
    }
    let s = state_flags();
    if card.flags_state & s.dead != 0 {
        return None;
    }
    if card.owner_id == 0 {
        return None;
    }
    cards::owning_soul(ctx, card.owner_id)
}

/// Apply a signed `±1` delta to `SoulPrivate.active_blueprints`
/// for `soul_card_id`. Saturating arithmetic on the `u8` field so
/// a stale-row replay can't underflow / overflow. No-op when the
/// soul has no `SoulPrivate` row yet (the row is created in
/// `players::spawn_soul_for` alongside the soul card, so this only
/// fires for hand-rolled / test-driven card writes without a real
/// soul behind them).
///
/// `SoulPrivate` is a flat-row table (no history), so this is a
/// delete + insert — same pattern as `PlayerProfile`'s
/// `lifecycle_count` updates in `players.rs`.
fn apply_blueprint_delta(ctx: &ReducerContext, soul_card_id: u32, delta: i8) {
    let Some(mut row) = ctx.db.soul_privates().card_id().find(soul_card_id) else {
        return;
    };
    row.active_blueprints = if delta > 0 {
        row.active_blueprints.saturating_add(1)
    } else {
        row.active_blueprints.saturating_sub(1)
    };
    ctx.db.soul_privates().card_id().delete(soul_card_id);
    ctx.db.soul_privates().insert(row);
}

// ---- the hook ------------------------------------------------------

/// Called from `cards::write_at` after every card insert. Maintains
/// the `Soul` table in lockstep with the cards table:
///
/// 1. **Soul-card identity sync.** If the written card is a soul
///    (`card_type == 6`), ensures a `Soul` row exists for its
///    `card_id` and mirrors the soul card's positional fields and
///    owner_id onto it. Auto-creates the row on first sight, so
///    `players::spawn_soul_for` doesn't need to manually manage Soul
///    rows — they appear as a side effect of writing the soul card.
///
/// 2. **Stat counters.** Computes the contribution of `prev_latest`
///    (the card row that was active before our write) and `new_card`
///    (the row we just wrote) toward their owners' Soul stat
///    counters. The delta — usually `0` (no stat impact), but
///    sometimes `-1` to one slot or `+1` to another or both —
///    is applied to the owners' Soul rows.
///
/// The hook is no-op for cards that aren't soul cards and don't map
/// to a tracked stat slot.
pub fn on_card_write(
    ctx: &ReducerContext,
    prev_latest: Option<&Card>,
    new_card: &Card,
    time_ms: u64,
) {
    // (3) Blueprint-counter diff. A live blueprint card
    // contributes +1 to its owning soul's
    // `SoulPrivate.active_blueprints`. Same delta model as the
    // stat-counter branch below: prev contribution -1, new
    // contribution +1, both no-op when same soul. Lives at the
    // top so soul-creation (in branch (1) below) is the only state
    // mutation other branches still depend on having seen.
    let prev_bp = prev_latest.and_then(|c| blueprint_contribution(ctx, c));
    let new_bp = blueprint_contribution(ctx, new_card);
    match (prev_bp, new_bp) {
        (Some(p), Some(n)) if p == n => {}
        (Some(p), Some(n)) => {
            apply_blueprint_delta(ctx, p, -1);
            apply_blueprint_delta(ctx, n, 1);
        }
        (Some(p), None) => apply_blueprint_delta(ctx, p, -1),
        (None, Some(n)) => apply_blueprint_delta(ctx, n, 1),
        (None, None) => {}
    }

    // (1) Identity sync. Only fires when the written card is a soul
    // card. Reads the *prior* soul row (max valid_at_time ≤ time_ms)
    // for the same reason `apply_slot_delta` does — out-of-order
    // writes within a reducer (e.g., movement followed by an earlier-
    // stamped action completion) must not contaminate older rows
    // with newer positional state. Auto-creates the Soul row on the
    // very first write of a given soul card.
    //
    // Future positional changes (later soul rows from movement) are
    // intentionally untouched here: each move_soul step has its own
    // (surface, macro_zone, ...) snapshot that we mustn't clobber.
    // The "stop on first deliberate change" forward-propagation rule
    // (used by zones) would apply if we ever did want to forward-
    // sync positional fields too.
    if is_soul_card(new_card.packed_definition) {
        let prior = ctx
            .db
            .souls()
            .card_id()
            .filter(new_card.card_id)
            .filter(|s| valid_at_time(s.valid_at) <= time_ms)
            .max_by_key(|s| valid_at_time(s.valid_at));
        match prior {
            Some(mut s) => {
                s.owner_id = new_card.owner_id;
                s.macro_zone = new_card.macro_zone;
                s.micro_location = new_card.micro_location;
                write_at(ctx, s, time_ms);
            }
            None => {
                write_at(
                    ctx,
                    Soul {
                        valid_at: 0,
                        data_shard: crate::DATA_SHARD,
                        card_id: new_card.card_id,
                        owner_id: new_card.owner_id,
                        macro_zone: new_card.macro_zone,
                        micro_location: new_card.micro_location,
                        stats: 0,
                        fatigued: 0,
                        injured: 0,
                    },
                    time_ms,
                );
            }
        }
    }

    // (2) Stat-counter diff. A card "contributes" to one Soul slot
    // when it's alive and maps to a tracked stat. Cards with the
    // `dead` bit set don't count. Owner / definition changes show up
    // here as a delta from one slot to another.
    let prev_contrib = prev_latest.and_then(|c| card_contribution(ctx, c));
    let new_contrib = card_contribution(ctx, new_card);
    match (prev_contrib, new_contrib) {
        (Some(p), Some(n)) if p == n => {
            // Same owner + same slot — no stat change.
        }
        (Some(p), Some(n)) => {
            apply_slot_delta(ctx, p.0, p.1, -1, time_ms);
            apply_slot_delta(ctx, n.0, n.1, 1, time_ms);
        }
        (Some(p), None) => apply_slot_delta(ctx, p.0, p.1, -1, time_ms),
        (None, Some(n)) => apply_slot_delta(ctx, n.0, n.1, 1, time_ms),
        (None, None) => {}
    }
}

/// What this card row contributes to a Soul's stat counters — the
/// soul that contains it, and the stat slot. Returns `None` for
/// dead cards, cards that don't map to a tracked slot, world-owned
/// cards (no soul in the chain), or soul cards themselves (a soul
/// doesn't contribute to its own stats).
///
/// Under the post-flag-20 card-owner model, the Soul row is keyed
/// by `card_id`, so we resolve the soul_card_id directly via
/// `cards::owning_soul` rather than going player → soul.
fn card_contribution(ctx: &ReducerContext, card: &Card) -> Option<(u32, StatSlot)> {
    let s = state_flags();
    if card.flags_state & s.dead != 0 {
        return None;
    }
    // Soul cards don't contribute to their own stats.
    if card.flags_state & s.is_owned_by_player != 0 {
        return None;
    }
    let slot = stat_slot_for(card.packed_definition)?;
    if card.owner_id == 0 {
        return None;
    }
    let soul_card_id = cards::owning_soul(ctx, card.owner_id)?;
    Some((soul_card_id, slot))
}

/// Apply a `±1` to one stat slot on the Soul row keyed by
/// `soul_card_id`. Silently skips when the soul has no row yet.
///
/// Two pieces of bookkeeping that mirror the zones forward-propagation
/// fix — both essential when card writes inside one reducer happen
/// out of chronological order (which they do: `action_completion::apply`
/// writes immediate rows, calls `on_create::trigger` on its products
/// which writes future-stamped rows, then returns to the outer apply
/// to write more immediate rows):
///
/// 1. **Read from the prior soul row, not the latest.** "Prior" is
///    the row with max `valid_at_time ≤ time_ms`. Reading
///    `latest()` (unbounded max) would pull in deltas applied by
///    later writes, contaminating our row with not-yet-due state.
///
/// 2. **Forward-propagate the delta.** Every soul row with
///    `valid_at_time > time_ms` represents the soul's state at
///    that future moment — and *that* state needs to include our
///    delta too, because the card we just touched is alive (or
///    newly-consumed) from `time_ms` onward. Unlike zones'
///    forward-propagation (which stops on deliberate change), souls
///    always propagate: each delta is an independent card-state
///    contribution, and downstream writes have already computed their
///    own deltas from card rows (not from the soul row), so we
///    aren't "double-counting" anything.
fn apply_slot_delta(
    ctx: &ReducerContext,
    soul_card_id: u32,
    slot: StatSlot,
    delta: i8,
    time_ms: u64,
) {
    if soul_card_id == 0 {
        return;
    }

    // (1) Read the prior soul row. Bail if no Soul exists yet — would
    // happen if a stat card is somehow written before the soul card
    // creates the Soul row via the identity-sync branch of
    // `on_card_write`. Today's call order guarantees the soul card is
    // always created before any owned faculty cards, but defending
    // here keeps the function honest.
    let Some(mut prior) = ctx
        .db
        .souls()
        .card_id()
        .filter(soul_card_id)
        .filter(|s| valid_at_time(s.valid_at) <= time_ms)
        .max_by_key(|s| valid_at_time(s.valid_at))
    else {
        return;
    };
    apply_delta_to_soul(&mut prior, slot, delta);
    write_at(ctx, prior, time_ms);

    // (2) Forward-propagate. Collect future row valid_ats first so
    // we're not iterating the table while mutating it.
    let mut future: Vec<u64> = ctx
        .db
        .souls()
        .card_id()
        .filter(soul_card_id)
        .filter(|s| valid_at_time(s.valid_at) > time_ms)
        .map(|s| s.valid_at)
        .collect();
    future.sort_unstable_by_key(|v| valid_at_time(*v));
    for v in future {
        let Some(mut s) = ctx.db.souls().valid_at().find(v) else {
            continue;
        };
        apply_delta_to_soul(&mut s, slot, delta);
        ctx.db.souls().valid_at().delete(v);
        ctx.db.souls().insert(s);
    }
}

/// Apply a signed `±1` delta to one byte of one packed-quad field on
/// `Soul`. Saturates at `[0, u8::MAX]`. Extracted as a helper because
/// `apply_slot_delta` uses it twice — once on the prior-row mutation,
/// once per future row during forward-propagation.
fn apply_delta_to_soul(s: &mut Soul, slot: StatSlot, delta: i8) {
    let field_value = match slot.field {
        StatField::Stats => s.stats,
        StatField::Fatigued => s.fatigued,
        StatField::Injured => s.injured,
    };
    let cur = quad_get(field_value, slot.byte_index);
    let next = if delta > 0 {
        cur.saturating_add(1)
    } else {
        cur.saturating_sub(1)
    };
    let updated = quad_set(field_value, slot.byte_index, next);
    match slot.field {
        StatField::Stats => s.stats = updated,
        StatField::Fatigued => s.fatigued = updated,
        StatField::Injured => s.injured = updated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quad_pack_round_trip() {
        let v = pack_quad(1, 2, 3, 4);
        assert_eq!(unpack_quad(v), (1, 2, 3, 4));
    }

    #[test]
    fn quad_get_set() {
        let v = pack_quad(10, 20, 30, 40);
        assert_eq!(quad_get(v, 0), 10);
        assert_eq!(quad_get(v, 1), 20);
        assert_eq!(quad_get(v, 2), 30);
        assert_eq!(quad_get(v, 3), 40);
        let v2 = quad_set(v, 1, 99);
        assert_eq!(quad_get(v2, 1), 99);
        assert_eq!(quad_get(v2, 0), 10);
        assert_eq!(quad_get(v2, 2), 30);
    }

    #[test]
    fn portrait_round_trip_and_isolation() {
        // Each portrait id reads back through the high-nibble.
        for id in 0u8..16 {
            assert_eq!(portrait_id(with_portrait(0, id)), id);
        }
        // Overlay onto an existing flag word leaves the lower bits alone.
        let base: u32 = (1 << 12) | (1 << 20) | 0b111 << 8;
        let v = with_portrait(base, 0xA);
        assert_eq!(portrait_id(v), 0xA);
        assert_eq!(v & !FLAG_PORTRAIT_MASK, base);
        // Out-of-range ids truncate to the low nibble (consistent with `quad_set`).
        assert_eq!(portrait_id(with_portrait(0, 0xFF)), 0xF);
    }
}
