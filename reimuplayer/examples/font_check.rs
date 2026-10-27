//! 一次性工具：核验随包图标字库的 cmap，找出「只在某张子表里、不在光栅器
//! 实际读取的那张里」的假可用码点（f365 `repeat-1` 就是这么变成空心豆腐的）。
//! 用法：`cargo run --example font_check -- <hex> <hex> ...`

fn u16(b: &[u8], at: usize) -> u32 {
    ((b[at] as u32) << 8) | b[at + 1] as u32
}
fn u32(b: &[u8], at: usize) -> u32 {
    ((b[at] as u32) << 24)
        | ((b[at + 1] as u32) << 16)
        | ((b[at + 2] as u32) << 8)
        | b[at + 3] as u32
}

/// 返回 (子表格式, 子表偏移)。
fn cmap_subtables(b: &[u8]) -> Vec<(u32, usize)> {
    let num_tables = u16(b, 4) as usize;
    let mut out = Vec::new();
    for i in 0..num_tables {
        let rec = 12 + i * 16;
        if u32(b, rec) != 0x636D6170 {
            continue; // 'cmap'
        }
        let off = u32(b, rec + 8) as usize;
        let sub_num = u16(b, off + 2) as usize;
        for s in 0..sub_num {
            let srec = off + 4 + s * 8;
            let so = u32(b, srec + 4) as usize + off;
            out.push((u16(b, so), so));
        }
    }
    out.sort();
    out.dedup();
    out
}

fn covers_format4(b: &[u8], so: usize, cp: u32) -> bool {
    let seg_x2 = u16(b, so + 6) as usize;
    let seg = seg_x2 / 2;
    let start_base = so + 14 + seg_x2 + 2;
    let delta_base = start_base + seg_x2;
    let range_base = delta_base + seg_x2;
    for i in 0..seg {
        let end = u16(b, so + 14 + i * 2);
        if cp > end {
            continue;
        }
        let start = u16(b, start_base + i * 2);
        if cp < start {
            continue;
        }
        let delta = u16(b, delta_base + i * 2);
        let range = u16(b, range_base + i * 2) as usize;
        if range == 0 {
            return ((cp).wrapping_add(delta) & 0xFFFF) != 0;
        }
        let addr = range_base + i * 2 + range + ((cp - start) as usize) * 2;
        let raw = u16(b, (addr + 1).min(b.len() - 1));
        return raw != 0 && ((raw.wrapping_add(delta)) & 0xFFFF) != 0;
    }
    false
}

fn covers_format12(b: &[u8], so: usize, cp: u32) -> bool {
    let n_groups = u32(b, so + 12) as usize;
    for i in 0..n_groups {
        let g = so + 16 + i * 12;
        if cp >= u32(b, g) && cp <= u32(b, g + 4) {
            return u32(b, g + 8) != 0;
        }
    }
    false
}

fn covers_format0(b: &[u8], so: usize, cp: u32) -> bool {
    cp < 256 && b.get(so + 6 + cp as usize).copied().unwrap_or(0) != 0
}

fn main() {
    let font = include_bytes!("../assets/fonts/fa-solid-900.ttf");
    let tables = cmap_subtables(font);
    println!(
        "cmap 子表: {:?}",
        tables
            .iter()
            .map(|(f, o)| (format!("format {f}"), *o))
            .collect::<Vec<_>>()
    );
    let mut cps: Vec<u32> = std::env::args()
        .skip(1)
        .filter_map(|a| u32::from_str_radix(a.trim().trim_start_matches("0x"), 16).ok())
        .collect();
    if cps.is_empty() {
        cps = vec![
            0xf04b, 0xf04c, 0xf048, 0xf051, 0xf074, 0xf363, 0xf365, 0xf061, 0xf002, 0xf005, 0xf03a,
            0xf51f, 0xf001, 0xf013, 0xf07c, 0xf028, 0xf052, 0xf2d1, 0xf2d0, 0xf2d2, 0xf00d, 0xf05a,
            0xf017, 0xf0c9, 0xf004, 0xf023, 0xf09c, 0xf21e, 0xf580, 0xf658, 0xf0eb, 0xf1fe, 0xf478,
            0xf231, 0xf0a1, 0xf12a,
        ];
    }
    for cp in cps {
        let per: Vec<String> = tables
            .iter()
            .map(|(f, o)| {
                let hit = match f {
                    0 => covers_format0(font, *o, cp),
                    4 => covers_format4(font, *o, cp),
                    12 => covers_format12(font, *o, cp),
                    _ => false,
                };
                format!("f{f}={}", if hit { "Y" } else { "n" })
            })
            .collect();
        println!("U+{cp:04X}  {}", per.join(" "));
    }
}
