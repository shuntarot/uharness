//! Reaching a region of `regs.json`: memory behind the window.
//!
//! A region is either fixed (the whole memory is in the window) or a moving
//! window: the window shows `size_bytes` of it at a time, and a base register
//! picks which page. There is one base register per master (`jtag`, `pcie`).

use std::collections::HashMap;

use hns_regs::{Region, RegisterMap};

use crate::bridge::{Batch, ReadHandle};
use crate::bus::Error;

/// Checks that `words` words at byte offset `at` of a region are backed by
/// real memory, and answers the window address of `at` (on page 0).
///
/// The limit comes from the entry count, not the window size (`size_bytes`).
/// A region is rounded up to a power of two. Past the entries, a read returns
/// 0 and bumps `<bundle>_oor`, and that 0 must not look like data.
pub fn span(region: &Region, at: u64, words: usize) -> Result<u32, Error> {
    let bytes = region.reach_bytes() as usize;
    if !at.is_multiple_of(4) {
        return Err(Error::Refused(format!(
            "0x{at:x} is not on a word boundary.\n\
             A region is read one 32-bit word at a time. Use 0x{:x} for the word it falls in.",
            at & !3
        )));
    }
    let end = at as usize + 4 * words.max(1);
    if end > bytes {
        return Err(Error::Refused(format!(
            "0x{at:x} + {words} word(s) runs past `{}`, which holds {bytes} bytes \
             ({} entries of {}).\n\
             Past the end, the memory returns 0 and counts the access in \
             `{}_oor`, so the value would look like data.",
            region.name, region.depth, region.entry_bytes, region.name
        )));
    }
    Ok((region.window_base + at as usize) as u32)
}

/// Splits a byte range of a region into pages: (page, window address,
/// words).
///
/// The window shows only `size_bytes` at a time, so a longer request is split,
/// and the base is set again for each part. A fixed window gives one part.
pub fn pages(region: &Region, at: u64, words: usize) -> Vec<(u64, u32, usize)> {
    let span = region.size_bytes as u64;
    let mut out = Vec::new();
    let mut left = words.max(1);
    let mut at = at;
    while left > 0 {
        let page = at / span;
        let off = at % span;
        let here = (((span - off) / 4) as usize).min(left);
        out.push((page, (region.window_base as u64 + off) as u32, here));
        at += 4 * here as u64;
        left -= here;
    }
    out
}

/// Queues the write that moves a moving window to `page`, for `master`.
/// Without it, the last page is read. A fixed window needs nothing.
pub fn set_page(
    b: &mut Batch,
    map: &RegisterMap,
    region: &Region,
    page: u64,
    master: &str,
) -> Result<(), Error> {
    if !region.is_aperture() {
        return Ok(());
    }
    let name = region.base_register(master);
    let base = map.get(&name).map_err(|_| {
        Error::Refused(format!(
            "`{}` is a moving window, and regs.json has no `{name}` to move it from the \
             {master} side.\n\
             Reach it over the transport whose base register the map has.",
            region.name
        ))
    })?;
    b.write(base.offset as u32, page as u32);
    Ok(())
}

/// A batch for a region.
///
/// For harness memory, it marks the batch replayable after a loss. Doing a
/// read or write twice gives the same result there. A DUT slave interface is
/// not marked, because a read alone may change its state.
pub fn batch(region: &Region) -> Batch {
    let mut b = Batch::new();
    if region.is_idempotent() {
        b.replayable();
    }
    b
}

/// Queues region accesses into a caller's batch, and moves a moving window
/// only when the page changes.
///
/// It remembers which page each window shows. Another master's moves do not
/// matter (each master has its own base register), but after a failed batch
/// the page is unknown: call `forget`.
pub struct Pager<'a> {
    map: &'a RegisterMap,
    master: &'static str,
    shown: HashMap<String, u64>,
}

impl<'a> Pager<'a> {
    /// `master` is `Bus::master()` of the bus the batches go to.
    pub fn new(map: &'a RegisterMap, master: &'static str) -> Self {
        Pager {
            map,
            master,
            shown: HashMap::new(),
        }
    }

    /// Forgets the pages, so the next access to each window sets its base.
    pub fn forget(&mut self) {
        self.shown.clear();
    }

    /// Queues reads of `words` words at byte offset `at` of `region`.
    pub fn read(
        &mut self,
        b: &mut Batch,
        region: &Region,
        at: u64,
        words: usize,
    ) -> Result<Vec<ReadHandle>, Error> {
        span(region, at, words)?;
        let mut out = Vec::with_capacity(words);
        for (page, addr, count) in pages(region, at, words) {
            self.show(b, region, page)?;
            out.extend((0..count).map(|i| b.read(addr + 4 * i as u32)));
        }
        Ok(out)
    }

    /// Queues writes of `values` at byte offset `at` of `region`.
    pub fn write(
        &mut self,
        b: &mut Batch,
        region: &Region,
        at: u64,
        values: &[u32],
    ) -> Result<(), Error> {
        span(region, at, values.len())?;
        let mut done = 0;
        for (page, addr, count) in pages(region, at, values.len()) {
            self.show(b, region, page)?;
            for (i, value) in values[done..done + count].iter().enumerate() {
                b.write(addr + 4 * i as u32, *value);
            }
            done += count;
        }
        Ok(())
    }

    fn show(&mut self, b: &mut Batch, region: &Region, page: u64) -> Result<(), Error> {
        if !region.is_aperture() || self.shown.get(&region.name) == Some(&page) {
            return Ok(());
        }
        set_page(b, self.map, region, page, self.master)?;
        self.shown.insert(region.name.clone(), page);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Op;

    fn region(size_bytes: usize, depth: u64, total_bytes: Option<u64>) -> Region {
        Region {
            kind: Some("memory".to_string()),
            total_bytes,
            held_in_dut_reset: false,
            name: "imem".to_string(),
            window_base: 0x200,
            size_bytes,
            entry_bytes: 4,
            depth,
        }
    }

    /// A read past a region's entries is refused before it is sent.
    ///
    /// A region is rounded up to a power of two, so part of it has no memory.
    /// A read there returns 0 and bumps `oor`, and that 0 must not look like
    /// data.
    #[test]
    fn a_read_past_the_end_of_a_region_is_refused() {
        // 100 entries x 4 bytes = 400 bytes, in a 512-byte window.
        let region = region(512, 100, None);

        // Inside, it passes, with `window_base` added.
        assert_eq!(span(&region, 0, 1).unwrap(), 0x200);
        assert_eq!(span(&region, 8, 2).unwrap(), 0x208);
        assert_eq!(span(&region, 396, 1).unwrap(), 0x200 + 396);

        // Past the last word.
        let err = span(&region, 396, 2).unwrap_err().to_string();
        assert!(err.contains("runs past"), "{err}");
        assert!(err.contains("400 bytes"), "{err}");
        // The error says a 0 would come back.
        assert!(err.contains("imem_oor"), "{err}");

        // The rounded-up part (400..512) is refused too, though in the window.
        assert!(span(&region, 400, 1).is_err());

        // An unaligned offset.
        let err = span(&region, 6, 1).unwrap_err().to_string();
        assert!(err.contains("word boundary"), "{err}");
        assert!(err.contains("0x4"), "{err}");
    }

    fn map_with_base() -> RegisterMap {
        let json = r#"{
            "marker": "veryl-harness:generated", "format_version": __VERSION__,
            "generator": "veryl-harness", "dut": "d", "word_bits": 32,
            "size_bytes": 4096, "magic": 1, "map_hash": 2,
            "registers": [{"name": "imem_base_jtag", "kind": "harness", "offset": 16,
                           "words": 1, "width": 32, "access": "rw", "bundle": "imem",
                           "value": null, "role": "base_jtag"}]
        }"#;
        RegisterMap::from_json(&json.replace("__VERSION__", &hns_regs::FORMAT_VERSION.to_string()))
            .unwrap()
    }

    /// The window moves only when the page changes, and a block that crosses
    /// a page edge moves it in the middle.
    #[test]
    fn a_pager_moves_the_window_only_when_the_page_changes() {
        let map = map_with_base();
        // A 16-byte window on 64 bytes of memory.
        let region = region(16, 16, Some(64));
        let mut pager = Pager::new(&map, "jtag");
        let mut b = Batch::new();
        // Words 2..6 cross from page 0 into page 1.
        pager.read(&mut b, &region, 8, 4).unwrap();
        // Page 1 again: no move.
        pager.read(&mut b, &region, 20, 1).unwrap();
        let ops: Vec<_> = b
            .ops()
            .iter()
            .map(|(f, _)| (f.op, f.addr, f.wdata))
            .collect();
        assert_eq!(
            ops,
            [
                (Op::Write, 16, 0),
                (Op::Read, 0x208, 0),
                (Op::Read, 0x20c, 0),
                (Op::Write, 16, 1),
                (Op::Read, 0x200, 0),
                (Op::Read, 0x204, 0),
                (Op::Read, 0x204, 0),
            ]
        );

        // After `forget`, the next access sets the page again.
        pager.forget();
        let mut b = Batch::new();
        pager.write(&mut b, &region, 20, &[7]).unwrap();
        assert_eq!(b.ops().len(), 2);
    }

    /// A moving window that has no base register for this master is refused,
    /// rather than reading whatever page the other master left.
    #[test]
    fn a_pager_refuses_a_master_without_a_base_register() {
        let map = map_with_base();
        let region = region(16, 16, Some(64));
        let mut pager = Pager::new(&map, "pcie");
        let err = pager
            .read(&mut Batch::new(), &region, 0, 1)
            .unwrap_err()
            .to_string();
        assert!(err.contains("imem_base_pcie"), "{err}");
    }
}
