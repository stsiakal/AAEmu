//! Bag contents tracking for the quest runner: decodes the server's item body
//! (`Item.Write`, `rust/crates/aaemu-game/src/models/item.rs`) and the item tasks of
//! `SCItemTaskSuccessPacket` (`models/item_task.rs`) into a slot-keyed bag map.
//!
//! The server sends the whole bag on character select (`SCCharacterInvenContentsPacket`,
//! 10 slots per packet) and every later change as item tasks (`systems/economy/`).
//! `Item.Write` is self-delimiting on the wire: the class detail block's length is determined
//! by the `DetailType` byte (`ItemClass::detail_bytes_length`). One caveat, mirrored from the
//! C#: a `SummonSlave` whose repair timer is not at `DateTime.MinValue` writes 8 instead of 4
//! bytes there; such a row would desynchronise its chunk (never observed in a bag).

use std::collections::BTreeMap;

use std::collections::HashMap;

use aaemu_commons::PacketStream;

/// One bag item: what the runner needs to count, select and use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemInfo {
    pub item_id: u64,
    pub template_id: u32,
    pub count: i32,
    /// Slot inside the bag (`slot_type::INVENTORY`).
    pub slot: u8,
}

/// Bag contents by slot (`slot_type::INVENTORY` = 2).
pub type Bag = BTreeMap<u8, ItemInfo>;

/// `SlotType.Inventory` (`models/container.rs`).
pub const SLOT_TYPE_INVENTORY: u8 = 2;

/// `ItemClass::detail_bytes_length` (`models/item.rs`): the detail block length by
/// `DetailType` (0 Invalid / 7 Decoration / 9 Glider / 10 SlaveEquipment / 11 Location write
/// nothing).
fn detail_len(detail_type: u8) -> usize {
    match detail_type {
        1 => 55, // Equipment (equip classes share the layout)
        2 => 29, // Slave
        3 => 6,  // Mate
        4 => 9,  // Ucc
        5 => 24, // Treasure
        6 => 16, // BigFish
        8 => 8,  // MusicSheet
        _ => 0,
    }
}

/// Reads one `Item.Write` body (`models/item.rs`). `None` for an empty slot (`i32 0`) or a
/// body-part row (only the template id; never part of the bag).
pub fn read_item(s: &mut PacketStream) -> anyhow::Result<Option<ItemInfo>> {
    let template_id = s.read_u32()?;
    if template_id == 0 {
        return Ok(None);
    }
    let item_id = s.read_u64()?;
    let _grade = s.read_u8()?;
    let _flags = s.read_u8()?;
    let count = s.read_i32()?;
    let detail_type = s.read_u8()?;
    s.read_bytes(detail_len(detail_type))?;
    let _create_time = s.read_i64()?;
    let _lifespan_mins = s.read_i32()?;
    let _made_unit_id = s.read_u32()?;
    let _world_id = s.read_u8()?;
    let _unsecure_time = s.read_i64()?;
    let _unpack_time = s.read_i64()?;
    Ok(Some(ItemInfo {
        item_id,
        template_id,
        count,
        slot: 0,
    }))
}

/// `(short)128` + 128 detail bytes (`ItemTask::write_padded_details`): the padded task block is
/// self-delimiting whatever the item class.
fn read_padded_details(s: &mut PacketStream) -> anyhow::Result<()> {
    let len = s.read_i16()?;
    anyhow::ensure!(len >= 0, "negative detail length {len}");
    s.read_bytes(len as usize)?;
    Ok(())
}

/// `ItemTask::write_task_item_details`: the full item without the class-specific length
/// problem. Returns `(item_id, template_id, count)`.
fn read_task_item_details(s: &mut PacketStream) -> anyhow::Result<(u64, u32, i32)> {
    let template_id = s.read_u32()?;
    let item_id = s.read_u64()?;
    let _grade = s.read_u8()?;
    let _flags = s.read_u8()?;
    let count = s.read_i32()?;
    read_padded_details(s)?;
    let _create_time = s.read_i64()?;
    let _lifespan_mins = s.read_i32()?;
    let _made_unit_id = s.read_u32()?;
    let _world_id = s.read_u8()?;
    let _unsecure_time = s.read_i64()?;
    let _unpack_time = s.read_i64()?;
    Ok((item_id, template_id, count))
}

/// Applies one `SCItemTaskSuccessPacket` body (`systems/economy/item_packets.rs` +
/// `models/item_task.rs`) to the bag. Only `INVENTORY` rows are tracked; a slot swap that moves
/// an item between the bag and an equipment slot is resolved with `item_templates` (the item id
/// -> template map fed by the create rows and the snapshot), because an `ItemMove` task carries
/// no template.
pub fn apply_item_task_success(
    bag: &mut Bag,
    item_templates: &mut HashMap<u64, u32>,
    body: &[u8],
) -> anyhow::Result<()> {
    let mut s = PacketStream::from_bytes(body.to_vec());
    let _action = s.read_u8()?;
    let count = s.read_u8()?;
    for _ in 0..count {
        apply_task(bag, item_templates, &mut s)?;
    }
    let forced = s.read_u8()?;
    for _ in 0..forced {
        s.read_u64()?; // force_remove item ids
    }
    s.read_u32()?; // type(id)
    Ok(())
}

fn apply_task(
    bag: &mut Bag,
    item_templates: &mut HashMap<u64, u32>,
    s: &mut PacketStream,
) -> anyhow::Result<()> {
    use aaemu_game::models::item_task::item_action;
    let action = s.read_u8()?;
    match action {
        item_action::CHANGE_MONEY_AMOUNT
        | item_action::CHANGE_BANK_MONEY_AMOUNT
        | item_action::CHANGE_AA_POINT
        | item_action::CHANGE_BANK_AA_POINT => {
            s.read_i32()?;
        }
        item_action::CHANGE_GAME_POINT => {
            s.read_u8()?;
            s.read_i32()?;
        }
        item_action::CHANGE_AUTO_USE_AA_POINT => {
            s.read_u8()?;
        }
        item_action::ADD_STACK => {
            let slot_type = s.read_u8()?;
            let slot = s.read_u8()?;
            let item_id = s.read_u64()?;
            let delta = s.read_i32()?;
            let template_id = s.read_u32()?;
            if slot_type == SLOT_TYPE_INVENTORY {
                // `ItemCountUpdate(item, count)` is a **delta** (`ItemContainer.cs`
                // `itemTasks.Add(new ItemCountUpdate(i, addAmount))`), positive for a stack
                // increase and negative for a consumption.
                match bag.get_mut(&slot) {
                    Some(entry) if entry.item_id == item_id => {
                        entry.count += delta;
                        if entry.count <= 0 {
                            bag.remove(&slot);
                        }
                    }
                    _ if delta > 0 => {
                        // No matching row yet (a missed snapshot): keep the delta.
                        bag.insert(
                            slot,
                            ItemInfo {
                                item_id,
                                template_id,
                                count: delta,
                                slot,
                            },
                        );
                    }
                    _ => {}
                }
            }
        }
        item_action::CREATE | item_action::TAKE | item_action::CHANGE_OWNER => {
            let slot_type = s.read_u8()?;
            let slot = s.read_u8()?;
            let (item_id, template_id, count) = read_task_item_details(s)?;
            item_templates.insert(item_id, template_id);
            if slot_type == SLOT_TYPE_INVENTORY {
                bag.insert(
                    slot,
                    ItemInfo {
                        item_id,
                        template_id,
                        count,
                        slot,
                    },
                );
            }
        }
        item_action::REMOVE => {
            let slot_type = s.read_u8()?;
            let slot = s.read_u8()?;
            let item_id = s.read_u64()?;
            let _template_id = s.read_u32()?;
            if slot_type == SLOT_TYPE_INVENTORY
                && bag.get(&slot).is_some_and(|i| i.item_id == item_id)
            {
                bag.remove(&slot);
            }
        }
        item_action::SWAP_SLOT => {
            let from_type = s.read_u8()?;
            let from_slot = s.read_u8()?;
            let to_type = s.read_u8()?;
            let to_slot = s.read_u8()?;
            let from_id = s.read_u64()?;
            let to_id = s.read_u64()?;
            let _flags = s.read_i32()?;
            // The task carries no templates: a bag -> bag swap of known rows is tracked, a move
            // out of the bag removes its row; the item that came the other way (an equipment
            // slot's previous item) is resolved through `item_templates`.
            let from = (from_type == SLOT_TYPE_INVENTORY)
                .then(|| bag.get(&from_slot).copied())
                .flatten()
                .filter(|i| i.item_id == from_id);
            let to = (to_type == SLOT_TYPE_INVENTORY)
                .then(|| bag.get(&to_slot).copied())
                .flatten()
                .filter(|i| i.item_id == to_id);
            let displaced = |templates: &HashMap<u64, u32>, item_id: u64, slot: u8| ItemInfo {
                item_id,
                template_id: templates.get(&item_id).copied().unwrap_or(0),
                count: 1,
                slot,
            };
            match (from, to) {
                (Some(f), Some(t)) => {
                    let mut f = f;
                    let mut t = t;
                    f.slot = to_slot;
                    t.slot = from_slot;
                    bag.insert(from_slot, t);
                    bag.insert(to_slot, f);
                }
                (Some(f), None) => {
                    bag.remove(&from_slot);
                    if to_type == SLOT_TYPE_INVENTORY {
                        let mut f = f;
                        f.slot = to_slot;
                        bag.insert(to_slot, f);
                    } else if to_id != 0 {
                        // The target slot had an item: it moves into the freed bag slot (the
                        // common two-handed-weapon swap).
                        bag.insert(from_slot, displaced(item_templates, to_id, from_slot));
                    }
                }
                (None, Some(t)) => {
                    bag.remove(&to_slot);
                    let _ = t; // the row's item moved out of the bag
                    if from_type != SLOT_TYPE_INVENTORY && from_id != 0 {
                        // An equipment item moved into the occupied bag slot.
                        bag.insert(to_slot, displaced(item_templates, from_id, to_slot));
                    }
                }
                (None, None) => {
                    // An equipment item moved into an empty bag slot (an empty row lookup: no
                    // bag row existed for either id).
                    if from_type != SLOT_TYPE_INVENTORY && to_type == SLOT_TYPE_INVENTORY {
                        bag.insert(to_slot, displaced(item_templates, from_id, to_slot));
                    }
                }
            }
        }
        item_action::UPDATE_DETAIL => {
            s.read_u8()?; // slot type
            s.read_u8()?; // slot
            s.read_u64()?; // item id
            read_padded_details(s)?;
        }
        item_action::SET_FLAGS_BITS => {
            s.read_u8()?;
            s.read_u8()?;
            s.read_u64()?;
            s.read_u8()?;
            s.read_u64()?;
        }
        item_action::UPDATE_FLAGS => {
            s.read_u8()?;
            s.read_u8()?;
            s.read_u64()?;
            s.read_u8()?;
            s.read_bool()?;
            s.read_bool()?;
            s.read_bool()?;
            s.read_i64()?;
            s.read_i64()?;
        }
        item_action::REMOVE_CRAFTING => {
            s.read_u64()?;
        }
        item_action::SEIZE => {
            let slot_type = s.read_u8()?;
            let slot = s.read_u8()?;
            let item_id = s.read_u64()?;
            if slot_type == SLOT_TYPE_INVENTORY
                && bag.get(&slot).is_some_and(|i| i.item_id == item_id)
            {
                bag.remove(&slot);
            }
        }
        item_action::CHANGE_GRADE => {
            s.read_u8()?;
            s.read_u8()?;
            s.read_u64()?;
            s.read_u8()?;
        }
        other => anyhow::bail!("unknown item task action {other}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use aaemu_commons::Marshal;
    use aaemu_game::models::item::{Item, ItemClass};
    use aaemu_game::models::item_task::ItemTask;
    use aaemu_game::opcodes::sc;
    use aaemu_game::packets::world::SCCharacterInvenContentsPacket;
    use aaemu_game::systems::economy::item_packets::SCItemTaskSuccessPacket;

    use super::*;
    use crate::game::{observe_into, GameState, Packet, LEVEL_GAME};

    fn packet(opcode: u16, body: Vec<u8>) -> Packet {
        Packet {
            level: LEVEL_GAME,
            opcode,
            body,
        }
    }

    fn item(template_id: u32, item_id: u64, slot: i32, count: i32) -> Item {
        let mut i = Item::new(1);
        i.class = ItemClass::Item;
        i.template_id = template_id;
        i.id = item_id;
        i.slot = slot;
        i.count = count;
        i.slot_type = SLOT_TYPE_INVENTORY;
        i
    }

    #[test]
    fn bag_snapshot_decodes_every_slot_of_a_chunk() {
        // `SCCharacterInvenContentsPacket::write` (`packets/world.rs`): type, chunks, chunk
        // index, then ten `Item.Write` rows (`i32 0` = empty slot).
        let mut body = PacketStream::new();
        SCCharacterInvenContentsPacket {
            slot_type: SLOT_TYPE_INVENTORY,
            num_chunks: 1,
            start_chunk_idx: 1,
            items: vec![
                None,
                Some(item(1000, 0x0102, 0, 3)),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(item(2000, 0x0203, 9, 1)),
            ],
        }
        .write(&mut body);
        let mut st = GameState::default();
        observe_into(
            &mut st,
            &packet(sc::SCCharacterInvenContentsPacket, body.into_bytes()),
        )
        .unwrap();
        assert_eq!(
            st.bag.get(&11).copied(),
            Some(ItemInfo {
                item_id: 0x0102,
                template_id: 1000,
                count: 3,
                slot: 11,
            })
        );
        assert_eq!(
            st.bag.get(&19).copied(),
            Some(ItemInfo {
                item_id: 0x0203,
                template_id: 2000,
                count: 1,
                slot: 19,
            })
        );
        assert_eq!(st.bag.len(), 2);
        assert_eq!(st.bag_count(&[1000, 2000]), 4);
        assert_eq!(st.bag_count(&[1000]), 3);
        assert_eq!(st.bag_count(&[9999]), 0);
    }

    #[test]
    fn item_task_add_count_and_remove_update_the_bag() {
        let add = ItemTask::add(&item(1000, 0x11, 5, 2));
        let mut body = PacketStream::new();
        SCItemTaskSuccessPacket {
            action: 9,
            tasks: vec![add],
            force_remove: vec![],
        }
        .write(&mut body);
        let mut bag = Bag::new();
        let mut templates = HashMap::new();
        apply_item_task_success(&mut bag, &mut templates, body.as_bytes()).unwrap();
        assert_eq!(
            bag.get(&5).map(|i| (i.template_id, i.count)),
            Some((1000, 2))
        );

        // `ItemCountUpdate` carries a delta: 2 + 5 = 7.
        let count = ItemTask::count_update(&item(1000, 0x11, 5, 2), 5);
        let mut body = PacketStream::new();
        SCItemTaskSuccessPacket {
            action: 9,
            tasks: vec![count],
            force_remove: vec![],
        }
        .write(&mut body);
        apply_item_task_success(&mut bag, &mut templates, body.as_bytes()).unwrap();
        assert_eq!(bag.get(&5).map(|i| i.count), Some(7));

        // A negative delta consumes the stack; the row disappears at <= 0.
        let count = ItemTask::count_update(&item(1000, 0x11, 5, 7), -7);
        let mut body = PacketStream::new();
        SCItemTaskSuccessPacket {
            action: 9,
            tasks: vec![count],
            force_remove: vec![],
        }
        .write(&mut body);
        apply_item_task_success(&mut bag, &mut templates, body.as_bytes()).unwrap();
        assert!(bag.is_empty());

        // The explicit remove task drops the matching row.
        let mut body = PacketStream::new();
        SCItemTaskSuccessPacket {
            action: 9,
            tasks: vec![ItemTask::add(&item(1000, 0x11, 5, 2))],
            force_remove: vec![],
        }
        .write(&mut body);
        apply_item_task_success(&mut bag, &mut templates, body.as_bytes()).unwrap();
        let mut body = PacketStream::new();
        SCItemTaskSuccessPacket {
            action: 9,
            tasks: vec![ItemTask::remove(&item(1000, 0x11, 5, 2))],
            force_remove: vec![],
        }
        .write(&mut body);
        apply_item_task_success(&mut bag, &mut templates, body.as_bytes()).unwrap();
        assert!(bag.is_empty());
    }

    #[test]
    fn loot_stacking_counts_the_full_amount_like_the_server_client_flow() {
        // The loot path (`looting.rs::try_distribute` -> `container.rs::acquire_default_item_ex`):
        // the first take creates the item (`ItemTask::add`, full count), later takes stack with
        // `ItemTask::count_update(item, add)` deltas (quest 50: 6x item 14635 in slot 4).
        let mut bag = Bag::new();
        let mut templates = HashMap::new();
        for take in 0..6 {
            let task = if take == 0 {
                ItemTask::add(&item(14_635, 0x0102_0304, 4, 1))
            } else {
                ItemTask::count_update(&item(14_635, 0x0102_0304, 4, 1), 1)
            };
            let mut body = PacketStream::new();
            SCItemTaskSuccessPacket {
                action: 10, // loot all
                tasks: vec![task],
                force_remove: vec![],
            }
            .write(&mut body);
            apply_item_task_success(&mut bag, &mut templates, body.as_bytes()).unwrap();
        }
        assert_eq!(bag.get(&4).map(|i| i.count), Some(6));
        let info = bag.get(&4).copied().unwrap();
        assert_eq!(info.template_id, 14_635);
        assert_eq!(info.item_id, 0x0102_0304);
    }

    #[test]
    fn item_task_swap_and_move_out_of_the_bag() {
        use aaemu_game::models::item_task::item_action;
        let mut bag = Bag::new();
        let mut templates = HashMap::new();
        bag.insert(
            5,
            ItemInfo {
                item_id: 0x11,
                template_id: 1000,
                count: 1,
                slot: 5,
            },
        );
        bag.insert(
            7,
            ItemInfo {
                item_id: 0x22,
                template_id: 2000,
                count: 1,
                slot: 7,
            },
        );

        // Bag 5 <-> bag 7 swap (the runner's `swap_items`).
        let mut s = PacketStream::new();
        s.write_u8(item_action::SWAP_SLOT)
            .write_u8(SLOT_TYPE_INVENTORY)
            .write_u8(5)
            .write_u8(SLOT_TYPE_INVENTORY)
            .write_u8(7)
            .write_u64(0x11)
            .write_u64(0x22)
            .write_i32(0);
        apply_task(&mut bag, &mut templates, &mut s).unwrap();
        assert_eq!(bag.get(&5).map(|i| i.item_id), Some(0x22));
        assert_eq!(bag.get(&7).map(|i| i.item_id), Some(0x11));

        // Bag -> equipment: the row leaves the bag.
        let mut s = PacketStream::new();
        s.write_u8(item_action::SWAP_SLOT)
            .write_u8(SLOT_TYPE_INVENTORY)
            .write_u8(5)
            .write_u8(1)
            .write_u8(0)
            .write_u64(0x22)
            .write_u64(0)
            .write_i32(0);
        apply_task(&mut bag, &mut templates, &mut s).unwrap();
        assert_eq!(bag.len(), 1);
        assert_eq!(bag.get(&7).map(|i| i.item_id), Some(0x11));

        // Bag -> equipment with an occupied target (the two-handed weapon swap): the displaced
        // equipment item lands in the freed bag slot; its template comes from the map.
        templates.insert(0x33, 3000);
        bag.insert(
            5,
            ItemInfo {
                item_id: 0x22,
                template_id: 2000,
                count: 1,
                slot: 5,
            },
        );
        let mut s = PacketStream::new();
        s.write_u8(item_action::SWAP_SLOT)
            .write_u8(SLOT_TYPE_INVENTORY)
            .write_u8(5)
            .write_u8(1)
            .write_u8(15)
            .write_u64(0x22)
            .write_u64(0x33)
            .write_i32(0);
        apply_task(&mut bag, &mut templates, &mut s).unwrap();
        assert_eq!(
            bag.get(&5).map(|i| (i.item_id, i.template_id)),
            Some((0x33, 3000))
        );

        // Equipment -> bag (the shield a two-handed weapon unequips): the row appears.
        templates.insert(0x44, 4000);
        let mut s = PacketStream::new();
        s.write_u8(item_action::SWAP_SLOT)
            .write_u8(1)
            .write_u8(16)
            .write_u8(SLOT_TYPE_INVENTORY)
            .write_u8(8)
            .write_u64(0x44)
            .write_u64(0)
            .write_i32(0);
        apply_task(&mut bag, &mut templates, &mut s).unwrap();
        assert_eq!(
            bag.get(&8).map(|i| (i.item_id, i.template_id)),
            Some((0x44, 4000))
        );
    }

    #[test]
    fn item_write_roundtrips_through_the_decoder() {
        // The server's own `Item::write` (an equip class with the 55-byte detail block).
        let mut equip = Item::new(1);
        equip.class = ItemClass::Armor;
        equip.template_id = 777;
        equip.id = 0x99;
        equip.count = 1;
        let mut body = PacketStream::new();
        equip.write(&mut body);
        let mut s = PacketStream::from_bytes(body.into_bytes());
        let info = read_item(&mut s).unwrap().unwrap();
        assert_eq!((info.template_id, info.item_id, info.count), (777, 0x99, 1));
        assert_eq!(s.remaining(), 0);

        // A `SummonSlave` with the default (MinValue) repair time writes the 29-byte block too.
        let mut slave = Item::new(1);
        slave.class = ItemClass::SummonSlave;
        slave.template_id = 888;
        slave.id = 0xAA;
        let mut body = PacketStream::new();
        slave.write(&mut body);
        let mut s = PacketStream::from_bytes(body.into_bytes());
        let info = read_item(&mut s).unwrap().unwrap();
        assert_eq!(info.template_id, 888);
        assert_eq!(s.remaining(), 0);
    }
}
