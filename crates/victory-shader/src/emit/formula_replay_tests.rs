//! Replays the face network's Formula program (0aa7934094794dbf, an
//! interpreter: 1,024 threads, a register file in a UAV, a group barrier
//! between its stages) on the host GPU from a dump of the title's buffers,
//! with its stage count cut, to read its registers after each stage.
//! Ignored; `VICTORY_FORMULA_DIR` names a folder with `formula.cdxil`,
//! `program.bin`, `input.bin`, `arena.bin`, `out.bin`, `consts.bin` and
//! `stages.txt` (the stage counts to run, the header half that holds it).
//! Writes `arena-<n>.bin` and `out-<n>.bin` there.

use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R16_FLOAT, DXGI_FORMAT_R16_UINT, DXGI_FORMAT_R16G16B16A16_UINT};

use super::host::{Dispatch, Host, Root, View};
use super::{SLOT_INTEGER, translate};

fn words(bytes: &[u8]) -> Vec<u32> {
    let mut b = bytes.to_vec();
    b.resize(b.len().next_multiple_of(4), 0);
    b.chunks_exact(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect()
}

#[test]
#[ignore]
fn formula_replay() {
    let Some(dir) = std::env::var_os("VICTORY_FORMULA_DIR").map(std::path::PathBuf::from) else {
        eprintln!("VICTORY_FORMULA_DIR unset");
        return;
    };
    let read = |n: &str| std::fs::read(dir.join(n)).unwrap_or_else(|e| panic!("{n}: {e}"));
    let shader = translate(&read("formula.cdxil")).expect("translate");
    let stages = String::from_utf8(read("stages.txt")).unwrap();
    let mut lines = stages.lines();
    let at: usize = lines.next().unwrap().trim().parse().unwrap();
    let counts: Vec<u16> = lines.filter_map(|l| l.trim().parse().ok()).collect();
    let input = words(&read("input.bin"));
    let arena = words(&read("arena.bin"));
    let out = words(&read("out.bin"));
    let consts = words(&read("consts.bin"));
    let warp = std::env::var_os("VICTORY_FORMULA_WARP").is_some();
    let host = Host::new(warp).expect("an SM 6.6 device");
    let (root, pso) = host.compute_pipeline(&shader).expect("pipeline");
    eprintln!("user data {:?} root srvs {:?}", shader.user_data, shader.root_srvs);
    // `VICTORY_FORMULA_SPLIT=<stages>`: one dispatch a stage instead, each
    // with `program-<k>.bin` (every thread's list cut to its stage k), the
    // arena and output carried from one to the next; writes
    // `arena-split-<k+1>.bin` and `out-split-<k+1>.bin`.
    if let Some(split) = std::env::var("VICTORY_FORMULA_SPLIT").ok().and_then(|v| v.parse::<u32>().ok()) {
        let (mut arena, mut out) = (arena.clone(), out.clone());
        for k in 0..split {
            let program = words(&read(&format!("program-{k}.bin")));
            let views = [
                View::TypedUavData { format: DXGI_FORMAT_R16G16B16A16_UINT, element_bytes: 8, data: &arena, share: None },
                View::TypedUavData { format: DXGI_FORMAT_R16G16B16A16_UINT, element_bytes: 8, data: &program, share: None },
                View::TypedUavData { format: DXGI_FORMAT_R16G16B16A16_UINT, element_bytes: 8, data: &program, share: Some(1) },
                View::TypedUavData { format: DXGI_FORMAT_R16_UINT, element_bytes: 2, data: &arena, share: Some(0) },
                View::TypedUavData { format: DXGI_FORMAT_R16G16B16A16_UINT, element_bytes: 8, data: &input, share: None },
                View::TypedUavData { format: DXGI_FORMAT_R16_UINT, element_bytes: 2, data: &out, share: None },
                View::TypedUavData { format: DXGI_FORMAT_R16_UINT, element_bytes: 2, data: &program, share: Some(1) },
                View::Cbv(&consts),
            ];
            let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
            for u in &shader.user_data {
                ud[u.index as usize] = 32 * u32::from(u.slot);
            }
            let mut roots = vec![(0, Root::Constants(ud))];
            if !shader.root_srvs.is_empty() {
                let i = SLOT_INTEGER;
                roots.push((1, Root::View { srv: true, data: vec![8 | i, 8 | i, 8 | i, 2 | i, 8 | i, 2 | i, 2 | i, 0] }));
            }
            let got = host
                .dispatch(&root, &pso, &Dispatch { roots, views: &views, samplers: &[], groups: [1, 1, 1] })
                .expect("dispatch");
            arena = got[0].clone();
            out = got[3].clone();
            let bytes = |w: &Vec<u32>| w.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>();
            std::fs::write(dir.join(format!("arena-split-{}.bin", k + 1)), bytes(&arena)).unwrap();
            std::fs::write(dir.join(format!("out-split-{}.bin", k + 1)), bytes(&out)).unwrap();
            eprintln!("split stage {k}: written");
        }
        return;
    }
    // `VICTORY_FORMULA_FLOAT=1`: the register file's R16 view as R16_FLOAT.
    let float = std::env::var_os("VICTORY_FORMULA_FLOAT").is_some();
    for &n in &counts {
        let mut program = read("program.bin");
        program[2 * at..2 * at + 2].copy_from_slice(&n.to_le_bytes());
        let program = words(&program);
        let views = [
            View::TypedUavData { format: DXGI_FORMAT_R16G16B16A16_UINT, element_bytes: 8, data: &arena, share: None },
            View::TypedUavData { format: DXGI_FORMAT_R16G16B16A16_UINT, element_bytes: 8, data: &program, share: None },
            View::TypedUavData { format: DXGI_FORMAT_R16G16B16A16_UINT, element_bytes: 8, data: &program, share: Some(1) },
            View::TypedUavData { format: if float { DXGI_FORMAT_R16_FLOAT } else { DXGI_FORMAT_R16_UINT }, element_bytes: 2, data: &arena, share: Some(0) },
            View::TypedUavData { format: DXGI_FORMAT_R16G16B16A16_UINT, element_bytes: 8, data: &input, share: None },
            View::TypedUavData { format: DXGI_FORMAT_R16_UINT, element_bytes: 2, data: &out, share: None },
            View::TypedUavData { format: DXGI_FORMAT_R16_UINT, element_bytes: 2, data: &program, share: Some(1) },
            View::Cbv(&consts),
        ];
        let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            ud[u.index as usize] = 32 * u32::from(u.slot);
        }
        let mut roots = vec![(0, Root::Constants(ud))];
        if !shader.root_srvs.is_empty() {
            let i = SLOT_INTEGER;
            let r16 = if float { 2 } else { 2 | i };
            roots.push((1, Root::View { srv: true, data: vec![8 | i, 8 | i, 8 | i, r16, 8 | i, 2 | i, 2 | i, 0] }));
        }
        let got = host
            .dispatch(&root, &pso, &Dispatch { roots, views: &views, samplers: &[], groups: [1, 1, 1] })
            .expect("dispatch");
        // UAV readbacks in heap order of the owning views: arena, program, input, out.
        let bytes = |w: &Vec<u32>| w.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>();
        std::fs::write(dir.join(format!("arena-{n}.bin")), bytes(&got[0])).unwrap();
        std::fs::write(dir.join(format!("out-{n}.bin")), bytes(&got[3])).unwrap();
        eprintln!("stages {n}: written");
    }
}
