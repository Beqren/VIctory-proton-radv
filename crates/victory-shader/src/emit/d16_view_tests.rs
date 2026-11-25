//! 16-bit typed buffer accesses (`_d16`) through integer and float views on
//! the host's GPU: whether an integer view's bits reach the program through
//! the float-typed handle the lowering uses (1adb4e0, after upstream
//! 4243a96, whose tests these are). The title's face network
//! (`compute_graph_ops`, e.g. 0aa7934094794dbf) keeps its halves in
//! R16G16B16A16_UINT and R16_UINT views.

use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_R16_FLOAT, DXGI_FORMAT_R16_UINT, DXGI_FORMAT_R16G16B16A16_FLOAT,
    DXGI_FORMAT_R16G16B16A16_UINT,
};

use super::host::{Dispatch, Host, Root, Stride, View};
use super::{HostShader, SLOT_INTEGER, SLOT_STRIDES, StaticSampler, lower};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

fn param(root: RootType, user_data: u8) -> RootParam {
    RootParam {
        root,
        table_offset_dwords: 0,
        sgprs: 1,
        base_register: 0,
        user_data,
        slot: user_data,
    }
}

/// An entry at offset 0 of the table in user data `user_data`.
fn entry(kind: ResourceType, dwords: u8, user_data: u8) -> UsedResource {
    UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords: 0,
        lower_bound: 0,
        stride: 0,
        dwords,
        user_data,
        slot: user_data,
    }
}

fn build(words: &[u32], layout: Vec<RootParam>, used: Vec<UsedResource>, threads: u32) -> HostShader {
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 8,
        layout,
        used,
        io: StageIo::Cs(ComputeIo {
            rsrc2: 3 << 1,
            threads: [threads, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    let io = io::raise(&p, &native).expect("io::raise");
    let lowered = lower::lower(&lower::Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    })
    .expect("lower");
    HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        user_data: lowered.user_data,
        static_samplers: lowered.samplers.iter().map(|&w| StaticSampler::from_words(w)).collect(),
        samplers: lowered.samplers,
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        root_uavs: lowered.root_uavs,
        root_indexed_views: res.root_indexed_views,
    }
}

/// User data: s0 a table with a typed buffer UAV, s1 a table with a raw
/// buffer of 2 dwords a lane, s2 a table with a raw UAV. Each lane stores
/// its 4 halves to element `lane` of the typed view (`_d16`), loads them
/// back the same way and stores what it read to dwords `2 lane, 2 lane + 1`
/// of the raw UAV.
const D16_ROUND_TRIP_WAVE32: &[u32] = &[
    0xBE84_0300, // s_mov_b32 s4, s0
    0xBE85_0380, // s_mov_b32 s5, 0
    0xF408_0202,
    0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
    0xBE86_0301, // s_mov_b32 s6, s1
    0xBE87_0380, // s_mov_b32 s7, 0
    0xF408_0303,
    0xFA00_0000, // s_load_dwordx4 s[12:15], s[6:7], 0x0
    0xBE94_0302, // s_mov_b32 s20, s2
    0xBE95_0380, // s_mov_b32 s21, 0
    0xF408_040A,
    0xFA00_0000, // s_load_dwordx4 s[16:19], s[20:21], 0x0
    0x3406_0083, // v_lshlrev_b32 v3, 3, v0
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0xE034_1000,
    0x8003_0103, // buffer_load_dwordx2 v[1:2], v3, s[12:15], 0 offen
    0xBF8C_3F70, // s_waitcnt vmcnt(0)
    0xE21C_2000,
    0x8002_0100, // buffer_store_format_d16_xyzw v[1:2], v0, s[8:11], 0 idxen
    0xE20C_2000,
    0x8002_0400, // buffer_load_format_d16_xyzw v[4:5], v0, s[8:11], 0 idxen
    0xBF8C_3F70, // s_waitcnt vmcnt(0)
    0xE074_1000,
    0x8004_0403, // buffer_store_dwordx2 v[4:5], v3, s[16:19], 0 offen
    0xBF81_0000, // s_endpgm
];

/// User data: s0 a table with a typed buffer SRV, s1 a table with a raw UAV.
/// Each lane loads element `lane` (`_d16_x`) and stores the dword it got to
/// dword `lane` of the raw UAV.
const D16_X_LOAD_WAVE32: &[u32] = &[
    0xBE84_0300, // s_mov_b32 s4, s0
    0xBE85_0380, // s_mov_b32 s5, 0
    0xF408_0202,
    0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
    0xBE86_0301, // s_mov_b32 s6, s1
    0xBE87_0380, // s_mov_b32 s7, 0
    0xF408_0403,
    0xFA00_0000, // s_load_dwordx4 s[16:19], s[6:7], 0x0
    0x3406_0082, // v_lshlrev_b32 v3, 2, v0
    0x7E08_0280, // v_mov_b32 v4, 0
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0xE200_2000,
    0x8002_0400, // buffer_load_format_d16_x v4, v0, s[8:11], 0 idxen
    0xBF8C_3F70, // s_waitcnt vmcnt(0)
    0xE070_1000,
    0x8004_0403, // buffer_store_dword v4, v3, s[16:19], 0 offen
    0xBF81_0000, // s_endpgm
];

/// Runs `shader` (user data: tables at heap indices 0, 1, 2 in order) on
/// hardware and WARP with `views` and the table by heap slot `strides`;
/// each adapter's UAV contents.
fn run_tables(shader: &HostShader, views: &[View], strides: Vec<u32>) -> Vec<(&'static str, Vec<Vec<u32>>)> {
    assert_eq!(shader.root_srvs, [SLOT_STRIDES]);
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = Vec::new();
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let (root, pso) = host
            .compute_pipeline(shader)
            .unwrap_or_else(|e| panic!("{label}: compute_pipeline failed: {e}"));
        let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            // Table k is the view at heap index k (32 bytes a view).
            ud[u.index as usize] = 32 * u32::from(u.slot);
        }
        let words = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud)), (1, Root::View { srv: true, data: strides.clone() })],
                    views,
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("{label}: dispatch failed: {e}"));
        out.push((label, words));
    }
    assert!(!out.is_empty(), "neither hardware nor WARP SM 6.6 device available");
    out
}

/// 16-bit typed stores and loads (`_d16`) through views of integer formats
/// pass the bits, and through float views convert as halves.
#[test]
fn d16_typed_buffer_accesses_pass_bits_through_integer_views() {
    let shader = build(
        D16_ROUND_TRIP_WAVE32,
        vec![param(RootType::Table, 0), param(RootType::Table, 1), param(RootType::Table, 2)],
        vec![
            entry(ResourceType::Uav, 4, 0),
            entry(ResourceType::Srv, 4, 1),
            entry(ResourceType::Uav, 4, 2),
        ],
        32,
    );
    // Lane l: negative zero and small, signalling NaN, denormal and
    // all-ones patterns as bits; normal halves for the float view.
    let bits: Vec<u32> = (0..32u32)
        .flat_map(|l| [(0x8000 | l) | (0x7C01 + l) << 16, (1 + l) | (0xFFFF - l) << 16])
        .collect();
    let halves: Vec<u32> = (0..32u32)
        .flat_map(|l| [(0x3C00 + l) | (0xC000 + 3 * l) << 16, (0x1400 + l) | (0x5BFF - l) << 16])
        .collect();
    let cases: [(DXGI_FORMAT, u32, &[u32]); 2] = [
        (DXGI_FORMAT_R16G16B16A16_UINT, 8 | SLOT_INTEGER, &bits),
        (DXGI_FORMAT_R16G16B16A16_FLOAT, 8, &halves),
    ];
    for (format, stride, data) in cases {
        let views = [
            View::TypedUav(format, 32, 8),
            View::Buffer { data, stride: Stride::Raw },
            View::RawUav(64),
        ];
        for (label, words) in run_tables(&shader, &views, vec![stride, 4, 4]) {
            assert_eq!(words[0], data, "{label} {format:?}: the typed view's bytes");
            assert_eq!(words[1], data, "{label} {format:?}: what the loads read");
        }
    }
}

/// A one-channel `_d16_x` load from a typed SRV: an integer view's 16 bits
/// as they are, a float view's half as it is.
#[test]
fn d16_x_loads_read_r16_uint_and_r16_float_views() {
    let shader = build(
        D16_X_LOAD_WAVE32,
        vec![param(RootType::Table, 0), param(RootType::Table, 1)],
        vec![entry(ResourceType::Srv, 4, 0), entry(ResourceType::Uav, 4, 1)],
        32,
    );
    let pack = |h: &dyn Fn(u32) -> u32| -> Vec<u32> { (0..16u32).map(|k| h(2 * k) | h(2 * k + 1) << 16).collect() };
    let uint = |l: u32| 0xF000 | l;
    let half = |l: u32| 0x3C00 + 5 * l;
    let cases: [(DXGI_FORMAT, u32, Vec<u32>, Vec<u32>); 2] = [
        (DXGI_FORMAT_R16_UINT, 2 | SLOT_INTEGER, pack(&uint), (0..32).map(uint).collect()),
        (DXGI_FORMAT_R16_FLOAT, 2, pack(&half), (0..32).map(half).collect()),
    ];
    for (format, stride, data, want) in cases {
        let views = [View::Buffer { data: &data, stride: Stride::Format(format, 2) }, View::RawUav(32)];
        for (label, words) in run_tables(&shader, &views, vec![stride, 4]) {
            assert_eq!(words[0], want, "{label} {format:?}");
        }
    }
}

/// A 32-bit `buffer_load_format_x` from an R16_UINT view (the head's
/// translation extract 970c4fbb3b017c0e at 0x370 reads the face network's
/// output so, then takes the low half as an f16 with `v_fma_mix_f32`): the
/// 16-bit integer, zero-extended.
#[test]
fn format_x_loads_read_r16_uint_views_as_integers() {
    let mut words = D16_X_LOAD_WAVE32.to_vec();
    // buffer_load_format_x v4, v0, s[8:11], 0 idxen (op 0, not 0x80)
    let at = words.iter().position(|&w| w == 0xE200_2000).expect("the load");
    words[at] = 0xE000_2000;
    let shader = build(
        &words,
        vec![param(RootType::Table, 0), param(RootType::Table, 1)],
        vec![entry(ResourceType::Srv, 4, 0), entry(ResourceType::Uav, 4, 1)],
        32,
    );
    let uint = |l: u32| 0xBC00 | l;
    let data: Vec<u32> = (0..16u32).map(|k| uint(2 * k) | uint(2 * k + 1) << 16).collect();
    let want: Vec<u32> = (0..32).map(uint).collect();
    let views = [View::Buffer { data: &data, stride: Stride::Format(DXGI_FORMAT_R16_UINT, 2) }, View::RawUav(32)];
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut ran = 0;
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            continue;
        };
        let (root, pso) = host.compute_pipeline(&shader).unwrap_or_else(|e| panic!("{label}: {e}"));
        let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            ud[u.index as usize] = 32 * u32::from(u.slot);
        }
        let mut roots = vec![(0, Root::Constants(ud))];
        if !shader.root_srvs.is_empty() {
            roots.push((1, Root::View { srv: true, data: vec![2 | SLOT_INTEGER, 4] }));
        }
        let got = host
            .dispatch(&root, &pso, &Dispatch { roots, views: &views, samplers: &[], groups: [1, 1, 1] })
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(got[0], want, "{label}");
        ran += 1;
    }
    assert!(ran > 0, "no SM 6.6 device");
}
