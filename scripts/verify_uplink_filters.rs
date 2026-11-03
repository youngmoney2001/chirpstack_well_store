// Standalone regression check for the uplink rx_info filter optimizations.
//
// It mirrors the patched functions from chirpstack/src/uplink/mod.rs with local
// stand-ins (no external crates) and compares them against the original upstream
// algorithms. This can be compiled with ONLY rustc -- no crates.io access needed:
//
//   rustc --edition 2024 -o /tmp/verify scripts/verify_uplink_filters.rs
//   /tmp/verify
//
// It checks three things that a syntax checker cannot:
//   1. the borrow checker accepts the disjoint field borrows inside Vec::retain;
//   2. type inference resolves `collect::<Result<_>>()?`, `copied()` and
//      `is_some_and`;
//   3. the patched filters produce exactly the same result as upstream.

// Standalone type/borrow-check harness for the two patched filters.
// Mirrors the exact code shapes used in chirpstack/src/uplink/mod.rs,
// with local stand-ins for lrwn::EUI64, uuid::Uuid, gw::UplinkRxInfo
// and anyhow::Result (no external crates needed).
use std::collections::HashMap;
use std::str::FromStr;

#[derive(Copy, Clone, PartialEq, Eq, Hash, Default, Debug)]
struct EUI64([u8; 8]);
impl FromStr for EUI64 {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if s.len() != 16 { return Err(format!("bad gateway id: {s}")); }
        Ok(EUI64([0; 8]))
    }
}
#[derive(Copy, Clone, PartialEq, Eq, Hash, Default, Debug)]
struct Uuid(u8);

#[derive(Clone, Debug, PartialEq)]
struct UplinkRxInfo { gateway_id: String }

#[derive(Default)]
struct UplinkFrameSet {
    rx_info_set: Vec<UplinkRxInfo>,
    gateway_private_up_map: HashMap<EUI64, bool>,
    gateway_tenant_id_map: HashMap<EUI64, Uuid>,
}

type Result<T> = std::result::Result<T, String>;

fn get_force_gws_private(_: &str) -> Result<bool> { Ok(false) }

// ---------- PATCHED (copied verbatim, stand-ins apart) ----------
fn filter_by_tenant_new(tenant_id: Uuid, uplink: &mut UplinkFrameSet) -> Result<()> {
    let force_gws_private = get_force_gws_private("eu868")?;
    let gateway_ids: Vec<EUI64> = uplink
        .rx_info_set
        .iter()
        .map(|rx_info| EUI64::from_str(&rx_info.gateway_id).map_err(|e| e.to_string()))
        .collect::<Result<_>>()?;

    let private_up_map = &uplink.gateway_private_up_map;
    let tenant_id_map = &uplink.gateway_tenant_id_map;

    let mut i = 0;
    uplink.rx_info_set.retain(|_| {
        let gateway_id = gateway_ids[i];
        i += 1;
        !(private_up_map.get(&gateway_id).copied().unwrap_or(true) || force_gws_private)
            || tenant_id_map
                .get(&gateway_id)
                .is_some_and(|v| *v == tenant_id)
    });
    if uplink.rx_info_set.is_empty() {
        return Err("RxInfo set is empty after applying filters".into());
    }
    Ok(())
}

fn filter_public_new(uplink: &mut UplinkFrameSet) -> Result<()> {
    let private_up_map = &uplink.gateway_private_up_map;
    let keep: Vec<bool> = uplink
        .rx_info_set
        .iter()
        .map(|rx_info| {
            let gateway_id = EUI64::from_str(&rx_info.gateway_id).map_err(|e| e.to_string())?;
            let private = *private_up_map
                .get(&gateway_id)
                .ok_or_else(|| "gateway_id missing in gateway_private_up_map".to_string())?;
            Ok(!private)
        })
        .collect::<Result<_>>()?;

    let mut i = 0;
    uplink.rx_info_set.retain(|_| {
        let keep_item = keep[i];
        i += 1;
        keep_item
    });
    if uplink.rx_info_set.is_empty() {
        return Err("rx_info_set is empty".into());
    }
    Ok(())
}

// ---------- ORIGINAL upstream algorithms (reference) ----------
fn filter_by_tenant_old(tenant_id: Uuid, uplink: &mut UplinkFrameSet) -> Result<()> {
    let force_gws_private = get_force_gws_private("eu868")?;
    let mut rx_info_set: Vec<UplinkRxInfo> = Vec::new();
    for rx_info in &uplink.rx_info_set {
        let gateway_id = EUI64::from_str(&rx_info.gateway_id).map_err(|e| e.to_string())?;
        if !(uplink.gateway_private_up_map.get(&gateway_id).cloned().unwrap_or(true)
            || force_gws_private)
            || uplink.gateway_tenant_id_map.get(&gateway_id).cloned().unwrap_or(Uuid(200)) == tenant_id
        {
            rx_info_set.push(rx_info.clone());
        }
    }
    uplink.rx_info_set = rx_info_set;
    if uplink.rx_info_set.is_empty() { return Err("RxInfo set is empty after applying filters".into()); }
    Ok(())
}

fn filter_public_old(uplink: &mut UplinkFrameSet) -> Result<()> {
    let mut rx_info_set: Vec<UplinkRxInfo> = Vec::new();
    for rx_info in &uplink.rx_info_set {
        let gateway_id = EUI64::from_str(&rx_info.gateway_id).map_err(|e| e.to_string())?;
        if !(*uplink.gateway_private_up_map.get(&gateway_id)
            .ok_or_else(|| "gateway_id missing in gateway_private_up_map".to_string())?)
        {
            rx_info_set.push(rx_info.clone());
        }
    }
    uplink.rx_info_set = rx_info_set;
    if uplink.rx_info_set.is_empty() { return Err("rx_info_set is empty".into()); }
    Ok(())
}

fn make(private: &[bool], tenants: &[Option<Uuid>]) -> UplinkFrameSet {
    let mut u = UplinkFrameSet::default();
    for (i, (p, t)) in private.iter().zip(tenants.iter()).enumerate() {
        let gw = EUI64([i as u8; 8]);
        u.rx_info_set.push(UplinkRxInfo { gateway_id: format!("{:016x}", i) });
        u.gateway_private_up_map.insert(gw, *p);
        if let Some(t) = t { u.gateway_tenant_id_map.insert(gw, *t); }
    }
    u
}

fn main() {
    let tenant = Uuid(1);
    let cases: Vec<(Vec<bool>, Vec<Option<Uuid>>)> = vec![
        (vec![false], vec![Some(tenant)]),
        (vec![false, true], vec![Some(tenant), Some(Uuid(2))]),
        (vec![true, true], vec![Some(tenant), None]),
        (vec![false, false], vec![None, Some(Uuid(3))]),
        (vec![true], vec![None]),
        (vec![false, false, true, false], vec![None, Some(tenant), Some(tenant), Some(Uuid(7))]),
    ];
    for (p, t) in cases {
        let (mut a, mut b) = (make(&p, &t), make(&p, &t));
        let ra = filter_by_tenant_new(tenant, &mut a);
        let rb = filter_by_tenant_old(tenant, &mut b);
        assert_eq!(ra.is_ok(), rb.is_ok(), "tenant: status mismatch");
        assert_eq!(a.rx_info_set, b.rx_info_set, "tenant: result mismatch");

        let (mut c, mut d) = (make(&p, &t), make(&p, &t));
        let rc = filter_public_new(&mut c);
        let rd = filter_public_old(&mut d);
        assert_eq!(rc.is_ok(), rd.is_ok(), "public: status mismatch");
        assert_eq!(c.rx_info_set, d.rx_info_set, "public: result mismatch");
    }

    // Malformed gateway id: error, and the original set is left untouched.
    let mut bad = make(&[false], &[Some(tenant)]);
    bad.rx_info_set[0].gateway_id = "oops".into();
    let before = bad.rx_info_set.clone();
    assert!(filter_by_tenant_new(tenant, &mut bad).is_err());
    assert_eq!(bad.rx_info_set, before, "set must be unchanged on error");
    assert!(filter_public_new(&mut bad).is_err());
    assert_eq!(bad.rx_info_set, before, "set must be unchanged on error");

    println!("OK: borrow/type check passed and patched filters match upstream behaviour");
}
