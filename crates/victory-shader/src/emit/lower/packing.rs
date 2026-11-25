//! Where parameters that pack 16-bit rows two per dword lay out their elements.

use std::collections::HashMap;

use crate::emit::container::SigElement;
use crate::native::StageIo;

/// The parameters packing 16-bit rows two per dword and where each lays
/// its elements out: (first row, [(dword, register, column, columns)]).
///
/// The console gives every 16-bit element whole dwords. A parameter leads
/// with the first element of its first row, then the first element of its
/// second row. An element that shares a register behind another one (the
/// signature packs a short element into the free columns of an earlier
/// register) is NOT in that row's halves: it follows in the first
/// parameter, in row order, that has whole dwords left.
/// The title's packed pedestrian shaders show both cases: TEXCOORD7 (register 1,
/// columns 2-3, behind the two columns of TEXCOORD0) is exported in dword 3
/// of parameter 0, after TEXCOORD1's two dwords; TEXCOORD6 (register 4,
/// column 3, behind the three columns of TEXCOORD4) in dword 3 of
/// parameter 3, after TEXCOORD5 and TEXCOORD7, while the high half of
/// TEXCOORD4's second dword stays zero.
type PackedLayout = Vec<(u8, Vec<(u8, u8, u8, u8)>)>;

fn packed_layout(elements: &[SigElement], packs: &[(u8, Option<u8>)]) -> PackedLayout {
    let of_row = |r: u8| {
        let mut of: Vec<&SigElement> = elements
            .iter()
            .filter(|e| e.row == Some(u32::from(r)))
            .collect();
        of.sort_by_key(|e| e.col);
        of
    };
    let mut packs = packs.to_vec();
    packs.sort_unstable();
    packs.dedup_by_key(|p| p.0);
    // Per parameter: its first row, the dwords used, the elements.
    let mut layout: Vec<(u8, u8, Vec<(u8, u8, u8, u8)>)> = Vec::new();
    let mut behind = Vec::new();
    for &(a, b) in &packs {
        let mut placed = Vec::new();
        let mut at = 0u8;
        if !of_row(a).is_empty() {
            for r in [Some(a), b].into_iter().flatten() {
                let of = of_row(r);
                let Some((lead, more)) = of.split_first() else {
                    continue;
                };
                placed.push((at, r, lead.col, lead.cols));
                at += lead.cols.div_ceil(2);
                behind.extend(more.iter().map(|e| (r, e.col, e.cols)));
            }
        }
        layout.push((a, at, placed));
    }
    behind.sort_unstable();
    for (r, col, cols) in behind {
        let need = cols.div_ceil(2);
        if let Some(p) = layout.iter_mut().find(|p| p.1 + need <= 4) {
            p.2.push((p.1, r, col, cols));
            p.1 += need;
        }
    }
    layout.into_iter().map(|(a, _, of)| (a, of)).collect()
}

/// Where half `h` (dword h / 2, the high half when odd) of the parameter
/// whose first 16-bit row is `a` comes from: a register and column of
/// `elements`, none for padding ([`packed_layout`]; `packs` lists every
/// parameter packing two 16-bit rows per dword).
pub(super) fn packed_half(
    elements: &[SigElement],
    packs: &[(u8, Option<u8>)],
    a: u8,
    h: u8,
) -> Option<(u8, u8)> {
    let layout = packed_layout(elements, packs);
    let (_, of) = layout.iter().find(|p| p.0 == a)?;
    of.iter().find_map(|&(dword, row, col, cols)| {
        let k = h.checked_sub(2 * dword)?;
        (k < cols).then_some((row, col + k))
    })
}

/// The pixel inputs packing two 16-bit rows per dword, as (first row,
/// second row) ([`packed_layout`]).
pub(super) fn input_packs(io: &StageIo, inputs: &[SigElement]) -> Vec<(u8, Option<u8>)> {
    let StageIo::Ps {
        inputs: entries, ..
    } = io
    else {
        return Vec::new();
    };
    let partners = half_input_partners(io, inputs);
    entries
        .iter()
        .filter(|i| i.fp16 && i.pack2)
        .map(|i| {
            let b = if i.row2 != i.row {
                Some(i.row2)
            } else {
                partners.get(&i.row).copied()
            };
            (i.row, b)
        })
        .collect()
}

/// Vertex or mesh outputs of 16-bit values (see [`Lower::half_outputs`]):
/// a packed row's partner is the next 16-bit row of the signature that no
/// output entry names, taken in row order.
pub(super) fn half_outputs(io: &StageIo, outputs: &[SigElement]) -> HashMap<u8, (bool, Option<u8>)> {
    let entries = match io {
        StageIo::Vs { outputs, .. } => outputs,
        StageIo::Ms(ms) => &ms.outputs,
        _ => return HashMap::new(),
    };
    let per_vertex = || entries.iter().filter(|o| o.param & 0x80 == 0);
    let mut free: Vec<u8> = outputs
        .iter()
        .filter(|e| e.half && e.kind == 0)
        .filter_map(|e| u8::try_from(e.row?).ok())
        .filter(|&r| !per_vertex().any(|o| o.row == r))
        .collect();
    free.sort_unstable();
    free.dedup();
    let mut halves: Vec<_> = per_vertex().filter(|o| o.fp16).collect();
    halves.sort_by_key(|o| o.row);
    halves
        .into_iter()
        .map(|o| {
            let b = o
                .pack2
                .then(|| free.iter().position(|&r| r > o.row))
                .flatten();
            (o.row, (o.pack2, b.map(|i| free.remove(i))))
        })
        .collect()
}

/// Pixel inputs packing two 16-bit rows per dword whose second row the
/// metadata leaves out (`row2` = `row`): as for outputs
/// ([`half_outputs`]), the next 16-bit row of the signature that no input
/// entry names, taken in row order (rows the metadata does give are taken
/// first).
pub(super) fn half_input_partners(io: &StageIo, inputs: &[SigElement]) -> HashMap<u8, u8> {
    let StageIo::Ps {
        inputs: entries, ..
    } = io
    else {
        return HashMap::new();
    };
    let packed = || entries.iter().filter(|i| i.fp16 && i.pack2);
    let mut free: Vec<u8> = inputs
        .iter()
        .filter(|e| e.half && e.kind == 0)
        .filter_map(|e| u8::try_from(e.row?).ok())
        .filter(|&r| !entries.iter().any(|i| i.row == r))
        .filter(|&r| !packed().any(|i| i.row2 != i.row && i.row2 == r))
        .collect();
    free.sort_unstable();
    free.dedup();
    let mut missing: Vec<_> = packed().filter(|i| i.row2 == i.row).collect();
    missing.sort_by_key(|i| i.row);
    let mut partners = HashMap::new();
    for i in missing {
        if let Some(k) = free.iter().position(|&r| r > i.row) {
            partners.insert(i.row, free.remove(k));
        }
    }
    partners
}
