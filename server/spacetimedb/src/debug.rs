use spacetimedb::ReducerContext;
use crate::cards::insert_panel_card_row;
use crate::definitions::find_def_by_str_id;
use crate::packing::CARD_FLAG_STACKABLE;

#[spacetimedb::reducer]
pub fn debug_spawn(
    ctx: &ReducerContext,
    owner_id: u32,
    card_id: String,
) -> Result<(), String> {
    let (card_type, definition_id) = find_def_by_str_id(&card_id)
        .ok_or_else(|| format!("unknown card id '{card_id}'"))?;
    insert_panel_card_row(ctx, card_type, 0, definition_id, owner_id, CARD_FLAG_STACKABLE)?;
    Ok(())
}
