use spacetimedb::table;

/// One version row per card state-change. `valid_at` is the unique
/// history key (packed `time_ms << 16 | seq`); `card_id` repeats
/// across a card's versions. A card's current state is the row with
/// the greatest `valid_at` that has elapsed.
#[table(accessor = cards, public)]
pub struct Card {
    #[primary_key]
    pub valid_at: u64,
    pub card_id: u32,
    pub macro_zone: u64,
    pub micro_zone: u32,
    pub flags: u32,
    pub book: u32,
    pub aspects: u32,
    pub owner: u32,
    pub definition: u16,
}
