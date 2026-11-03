use std::collections::HashMap;
use std::convert::{TryFrom, TryInto};
use std::io::Cursor;
use std::str::FromStr;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prost::Message;
use tokio::time::sleep;
use tracing::{Instrument, Level, debug, error, info, span, trace, warn};
use uuid::Uuid;

use crate::config;
use crate::helpers::errors::PrintFullError;
use crate::monitoring::prometheus;
use crate::storage::{
    device, device_profile, error::Error as StorageError, gateway, get_async_redis_conn, redis_key,
};
use crate::stream;
use chirpstack_api::{common, gw, stream as stream_pb};
use lrwn::region::CommonName;
use lrwn::{EUI64, FType, ForwardUplinkReq, PhyPayload};

mod data;
mod data_fns;
pub mod data_sns;
pub mod error;
pub mod helpers;
pub mod join;
pub mod join_fns;
pub mod join_sns;
pub mod mesh;
pub mod stats;

#[derive(Clone, Hash, PartialEq, Eq, EncodeLabelSet, Debug)]
struct UplinkLabels {
    f_type: String,
}

static UPLINK_COUNTER: LazyLock<Family<UplinkLabels, Counter>> = LazyLock::new(|| {
    let counter = Family::<UplinkLabels, Counter>::default();
    prometheus::register(
        "uplink_count",
        "Number of received uplinks (after deduplication)",
        counter.clone(),
    );
    counter
});
static DEDUPLICATE_LOCKED_COUNTER: LazyLock<Counter> = LazyLock::new(|| {
    let counter = Counter::default();
    prometheus::register(
        "deduplicate_locked_count",
        "Number of times the deduplication function was called and the deduplication was already locked",
        counter.clone(),
    );
    counter
});
static DEDUPLICATE_NO_LOCK_COUNTER: LazyLock<Counter> = LazyLock::new(|| {
    let counter = Counter::default();
    prometheus::register(
        "deduplicate_no_lock_count",
        "Number of times the deduplication function was called and it was not yet locked",
        counter.clone(),
    );
    counter
});

#[derive(Clone)]
pub struct RelayContext {
    pub req: ForwardUplinkReq,
    pub device: device::Device,
    pub device_profile: device_profile::DeviceProfile,
    pub must_ack: bool,
    pub must_send_downlink: bool,
}

#[derive(Clone)]
pub struct UplinkFrameSet {
    pub uplink_set_id: Uuid,
    pub dr: u8,
    pub ch: usize,
    pub phy_payload: PhyPayload,
    pub tx_info: gw::UplinkTxInfo,
    pub rx_info_set: Vec<gw::UplinkRxInfo>,
    pub gateway_private_up_map: HashMap<EUI64, bool>,
    pub gateway_private_down_map: HashMap<EUI64, bool>,
    pub gateway_tenant_id_map: HashMap<EUI64, Uuid>,
    pub gateway_downlink_priority_map: HashMap<EUI64, u32>,
    pub region_common_name: CommonName,
    pub region_config_id: String,
    pub roaming_meta_data: Option<RoamingMetaData>,
}

impl TryFrom<&UplinkFrameSet> for stream_pb::UplinkFrameLog {
    type Error = anyhow::Error;

    fn try_from(
        ufs: &UplinkFrameSet,
    ) -> std::result::Result<stream_pb::UplinkFrameLog, Self::Error> {
        let mut ufl = stream_pb::UplinkFrameLog {
            phy_payload: ufs.phy_payload.to_vec()?,
            tx_info: Some(ufs.tx_info.clone()),
            rx_info: ufs.rx_info_set.clone(),
            f_type: match ufs.phy_payload.mhdr.f_type {
                lrwn::FType::JoinRequest => common::FType::JoinRequest,
                lrwn::FType::RejoinRequest => common::FType::RejoinRequest,
                lrwn::FType::UnconfirmedDataUp => common::FType::UnconfirmedDataUp,
                lrwn::FType::ConfirmedDataUp => common::FType::ConfirmedDataUp,
                lrwn::FType::Proprietary => common::FType::Proprietary,
                _ => {
                    return Err(anyhow!(
                        "Unexpected f_type: {}",
                        ufs.phy_payload.mhdr.f_type
                    ));
                }
            }
            .into(),
            dev_addr: match &ufs.phy_payload.payload {
                lrwn::Payload::MACPayload(v) => v.fhdr.devaddr.to_string(),
                _ => "".to_string(),
            },
            dev_eui: match &ufs.phy_payload.payload {
                lrwn::Payload::JoinRequest(v) => v.dev_eui.to_string(),
                _ => "".to_string(),
            },
            time: None, // is set below
            plaintext_f_opts: false,
            plaintext_frm_payload: false,
        };

        for rx_info in &ufl.rx_info {
            if rx_info.gw_time.is_some() {
                ufl.time = rx_info.gw_time.as_ref().map(|t| pbjson_types::Timestamp {
                    seconds: t.seconds,
                    nanos: t.nanos,
                });
            }
        }

        if ufl.time.is_none() {
            ufl.time = Some(Utc::now().into());
        }

        Ok(ufl)
    }
}

#[derive(Clone)]
pub struct RoamingMetaData {
    pub base_payload: backend::BasePayload,
    pub ul_meta_data: backend::ULMetaData,
}

pub async fn deduplicate_uplink(
    region_common_name: CommonName,
    region_config_id: String,
    event: gw::UplinkFrame,
) {
    if let Err(e) = _deduplicate_uplink(region_common_name, &region_config_id, event).await {
        error!(error = %e.full(), "Deduplication error");
    }
}

async fn _deduplicate_uplink(
    region_common_name: CommonName,
    region_config_id: &str,
    event: gw::UplinkFrame,
) -> Result<()> {
    let phy_str = hex::encode(&event.phy_payload);
    let tx_info_str = match &event.tx_info {
        Some(tx_info) => hex::encode(tx_info.encode_to_vec()),
        None => "".to_string(),
    };

    let key = redis_key(format!(
        "up:collect:{{{}:{}:{}}}",
        region_config_id, tx_info_str, phy_str
    ));
    let lock_key = redis_key(format!(
        "up:collect:{{{}:{}:{}}}:lock",
        region_config_id, tx_info_str, phy_str
    ));

    let dedup_delay = config::get().network.deduplication_delay;
    let mut dedup_ttl = dedup_delay * 2;
    if dedup_ttl < Duration::from_millis(200) {
        dedup_ttl = Duration::from_millis(200);
    }

    trace!(
        key = key.as_str(),
        "Adding uplink event to deduplication set and getting lock"
    );
    let locked = deduplicate_put(&key, &lock_key, dedup_ttl, &event).await?;
    if locked {
        trace!(
            lock_key = lock_key.as_str(),
            "Deduplication is already locked by an other process"
        );

        DEDUPLICATE_LOCKED_COUNTER.inc();

        return Ok(());
    }

    DEDUPLICATE_NO_LOCK_COUNTER.inc();

    trace!(
        key = key.as_str(),
        "Waiting for more uplink events to receive"
    );
    sleep(dedup_delay).await;

    trace!(key = key.as_str(), "Collecting received uplink events");
    let uplink = deduplicate_collect(&key).await?;

    let deduplication_id = Uuid::new_v4();
    let span = span!(Level::INFO, "up", deduplication_id = %deduplication_id);
    handle_uplink(
        region_common_name,
        region_config_id,
        deduplication_id,
        uplink,
    )
    .instrument(span)
    .await?;

    Ok(())
}

async fn deduplicate_put(
    collect_key: &str,
    lock_key: &str,
    ttl: Duration,
    event: &gw::UplinkFrame,
) -> Result<bool> {
    let event_b = event.encode_to_vec();

    let (lock_set,): (bool,) = redis::pipe()
        .atomic()
        .cmd("SADD")
        .arg(collect_key)
        .arg(event_b)
        .ignore()
        .cmd("PEXPIRE")
        .arg(collect_key)
        .arg(ttl.as_millis() as usize)
        .ignore()
        .cmd("SET")
        .arg(lock_key)
        .arg("lock")
        .arg("PX")
        .arg(ttl.as_millis() as usize)
        .arg("NX")
        .query_async(&mut get_async_redis_conn().await?)
        .await
        .context("Deduplication put and get lock")?;

    // We get true if we were able to set the lock, thus true == not yet locked.
    Ok(!lock_set)
}

async fn deduplicate_collect(key: &str) -> Result<gw::UplinkFrameSet> {
    let items_b: Vec<Vec<u8>> = {
        redis::cmd("SMEMBERS")
            .arg(key)
            .query_async(&mut get_async_redis_conn().await?)
            .await
            .context("Deduplication collect")?
    };

    if items_b.is_empty() {
        return Err(anyhow!("Zero items in collect set"));
    }

    let mut pl = gw::UplinkFrameSet {
        ..Default::default()
    };

    for b in items_b {
        let event = gw::UplinkFrame::decode(&mut Cursor::new(b)).context("Decode UplinkFrame")?;

        if event.tx_info.is_none() {
            warn!("tx_info of uplink event is empty, skipping");
            continue;
        }
        if event.rx_info.is_none() {
            warn!("rx_info of uplink event is empty, skipping");
            continue;
        }

        pl.tx_info = event.tx_info;
        pl.rx_info.push(event.rx_info.unwrap());
        pl.phy_payload = event.phy_payload;
    }

    Ok(pl)
}

pub async fn handle_uplink(
    region_common_name: CommonName,
    region_config_id: &str,
    deduplication_id: Uuid,
    uplink: gw::UplinkFrameSet,
) -> Result<()> {
    let mut uplink = UplinkFrameSet {
        uplink_set_id: deduplication_id,
        region_common_name,
        region_config_id: region_config_id.to_string(),
        dr: 0,
        ch: 0,
        phy_payload: PhyPayload::from_slice(&uplink.phy_payload)?,
        tx_info: uplink.tx_info.context("tx_info must not be None")?,
        rx_info_set: uplink.rx_info,
        gateway_private_up_map: HashMap::new(),
        gateway_private_down_map: HashMap::new(),
        gateway_tenant_id_map: HashMap::new(),
        gateway_downlink_priority_map: HashMap::new(),
        roaming_meta_data: None,
    };

    UPLINK_COUNTER
        .get_or_create(&UplinkLabels {
            f_type: uplink.phy_payload.mhdr.f_type.to_string(),
        })
        .inc();

    uplink.dr = helpers::get_uplink_dr(&uplink.region_config_id, &uplink.tx_info)?;
    uplink.ch = helpers::get_uplink_ch(
        &uplink.region_config_id,
        uplink.tx_info.frequency,
        uplink.dr,
    )?;

    info!(
        f_type = %uplink.phy_payload.mhdr.f_type,
        "Uplink received"
    );

    debug!("Updating gateway meta-data for uplink frame-set");
    update_gateway_metadata(&mut uplink)
        .await
        .context("Update gateway meta-data")?;

    debug!("Logging uplink frame to Redis Stream");
    let ufl: stream_pb::UplinkFrameLog = (&uplink).try_into()?;
    stream::frame::log_uplink_for_gateways(&ufl)
        .await
        .context("Log uplink for gateways")?;

    match uplink.phy_payload.mhdr.f_type {
        FType::JoinRequest => join::JoinRequest::handle(uplink).await,
        FType::UnconfirmedDataUp | FType::ConfirmedDataUp => data::Data::handle(uplink).await,
        _ => {
            return Err(anyhow!(
                "Unexpected f_type: {}",
                uplink.phy_payload.mhdr.f_type
            ));
        }
    }

    Ok(())
}

async fn update_gateway_metadata(ufs: &mut UplinkFrameSet) -> Result<()> {
    let conf = config::get();
    for rx_info in &mut ufs.rx_info_set {
        let gw_id = EUI64::from_str(&rx_info.gateway_id).context("Gateway ID")?;
        let gw_meta = match gateway::get_meta(&gw_id).await {
            Ok(v) => v,
            Err(e) => {
                if conf.gateway.allow_unknown_gateways
                    && let StorageError::NotFound(_) = e
                {
                    ufs.gateway_private_up_map.insert(gw_id, false);
                    ufs.gateway_private_down_map.insert(gw_id, false);
                    ufs.gateway_downlink_priority_map
                        .insert(gw_id, conf.gateway.unknown_gateway_downlink_priority);
                    continue;
                }

                error!(
                    gateway_id = %gw_id,
                    error = %e.full(),
                    "Getting gateway meta-data failed"
                );
                continue;
            }
        };

        // Do not overwrite the location if it is already set. In case of a 'virtual' it is
        // possible that the location is already set in the RxInfo. Overwriting this with the
        // location of the 'virtual' gateway would mean we will get the wrong location.
        if rx_info.location.is_none() {
            rx_info.location = Some(common::Location {
                latitude: gw_meta.latitude,
                longitude: gw_meta.longitude,
                altitude: gw_meta.altitude as f64,
                ..Default::default()
            });
        }

        ufs.gateway_private_up_map
            .insert(gw_id, gw_meta.is_private_up);
        ufs.gateway_private_down_map
            .insert(gw_id, gw_meta.is_private_down);
        ufs.gateway_tenant_id_map
            .insert(gw_id, gw_meta.tenant_id.into());
        ufs.gateway_downlink_priority_map
            .insert(gw_id, gw_meta.downlink_priority as u32);
    }

    Ok(())
}

fn filter_rx_info_by_tenant_id(tenant_id: Uuid, uplink: &mut UplinkFrameSet) -> Result<()> {
    let force_gws_private = config::get_force_gws_private(&uplink.region_config_id)?;

    // Resolve everything that can fail before modifying rx_info_set, so that a
    // malformed gateway id leaves the original set untouched (as before).
    // Filtering in place avoids cloning every UplinkRxInfo into a second Vec:
    // on a busy network this removes one deep protobuf clone per receiving
    // gateway, for every uplink.
    let gateway_ids: Vec<EUI64> = uplink
        .rx_info_set
        .iter()
        .map(|rx_info| EUI64::from_str(&rx_info.gateway_id).context("Gateway ID"))
        .collect::<Result<_>>()?;

    let private_up_map = &uplink.gateway_private_up_map;
    let tenant_id_map = &uplink.gateway_tenant_id_map;

    let mut i = 0;
    uplink.rx_info_set.retain(|_| {
        let gateway_id = gateway_ids[i];
        i += 1;

        // A gateway without a tenant_id must never match. A freshly generated
        // random UUID was used for this before; checking the Option directly
        // removes one RNG call per gateway and makes the behaviour deterministic.
        !(private_up_map.get(&gateway_id).copied().unwrap_or(true) || force_gws_private)
            || tenant_id_map
                .get(&gateway_id)
                .is_some_and(|v| *v == tenant_id)
    });

    if uplink.rx_info_set.is_empty() {
        return Err(anyhow!("RxInfo set is empty after applying filters"));
    }

    Ok(())
}

fn filter_rx_info_by_public_only(uplink: &mut UplinkFrameSet) -> Result<()> {
    // Same approach as above: pre-resolve ids + flags so errors are returned
    // before any mutation, then compact the Vec in place (no protobuf clones).
    let private_up_map = &uplink.gateway_private_up_map;
    let keep: Vec<bool> = uplink
        .rx_info_set
        .iter()
        .map(|rx_info| {
            let gateway_id = EUI64::from_str(&rx_info.gateway_id).context("Gateway ID")?;
            let private = *private_up_map
                .get(&gateway_id)
                .ok_or_else(|| anyhow!("gateway_id missing in gateway_private_up_map"))?;
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
        return Err(anyhow!("rx_info_set is empty"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Minimal valid UnconfirmedDataUp frame:
    // MHDR + FHDR (DevAddr 4 + FCtrl 1 + FCnt 2) + FPort + MIC (4).
    fn test_phy_payload() -> PhyPayload {
        PhyPayload::from_slice(&[
            0x40, 0x01, 0x02, 0x03, 0x04, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
        ])
        .unwrap()
    }

    fn set_test_config() {
        let mut conf = config::Configuration::default();
        conf.regions = vec![config::Region {
            id: "eu868".into(),
            ..Default::default()
        }];
        config::set(conf);
    }

    fn gateway_id(i: u8) -> String {
        format!("{:016x}", i)
    }

    /// Builds an uplink frame set with `n` gateways. `private` and `tenant_id`
    /// describe what the maps contain for each gateway.
    fn frame_set(n: u8, private: &[bool], tenant_ids: &[Option<Uuid>]) -> UplinkFrameSet {
        let mut ufs = UplinkFrameSet {
            uplink_set_id: Uuid::new_v4(),
            dr: 0,
            ch: 0,
            phy_payload: test_phy_payload(),
            tx_info: Default::default(),
            rx_info_set: Vec::new(),
            gateway_private_up_map: HashMap::new(),
            gateway_private_down_map: HashMap::new(),
            gateway_tenant_id_map: HashMap::new(),
            gateway_downlink_priority_map: HashMap::new(),
            region_common_name: CommonName::EU868,
            region_config_id: "eu868".into(),
            roaming_meta_data: None,
        };

        for i in 0..n {
            let id = gateway_id(i);
            let gw_id = EUI64::from_str(&id).unwrap();
            ufs.rx_info_set.push(gw::UplinkRxInfo {
                gateway_id: id,
                ..Default::default()
            });
            ufs.gateway_private_up_map
                .insert(gw_id, *private.get(i as usize).unwrap_or(&true));
            if let Some(Some(t)) = tenant_ids.get(i as usize) {
                ufs.gateway_tenant_id_map.insert(gw_id, *t);
            }
        }

        ufs
    }

    /// The original (upstream v4.20.0) implementation, used as a reference:
    /// it cloned every retained UplinkRxInfo into a new Vec.
    fn tenant_filter_reference(tenant_id: Uuid, ufs: &UplinkFrameSet) -> Vec<String> {
        let mut out = Vec::new();
        for rx_info in &ufs.rx_info_set {
            let gw_id = EUI64::from_str(&rx_info.gateway_id).unwrap();
            if !(ufs
                .gateway_private_up_map
                .get(&gw_id)
                .cloned()
                .unwrap_or(true)
                || false)
                || ufs
                    .gateway_tenant_id_map
                    .get(&gw_id)
                    .cloned()
                    .unwrap_or_else(Uuid::new_v4)
                    == tenant_id
            {
                out.push(rx_info.gateway_id.clone());
            }
        }
        out
    }

    fn public_filter_reference(ufs: &UplinkFrameSet) -> Vec<String> {
        let mut out = Vec::new();
        for rx_info in &ufs.rx_info_set {
            let gw_id = EUI64::from_str(&rx_info.gateway_id).unwrap();
            if !(*ufs.gateway_private_up_map.get(&gw_id).unwrap()) {
                out.push(rx_info.gateway_id.clone());
            }
        }
        out
    }

    fn ids(ufs: &UplinkFrameSet) -> Vec<String> {
        ufs.rx_info_set
            .iter()
            .map(|v| v.gateway_id.clone())
            .collect()
    }

    #[test]
    fn tenant_filter_matches_upstream_behaviour() {
        set_test_config();
        let tenant = Uuid::new_v4();
        let other = Uuid::new_v4();

        let cases: Vec<(Vec<bool>, Vec<Option<Uuid>>)> = vec![
            (vec![false], vec![Some(tenant)]),
            (vec![false, true], vec![Some(tenant), Some(other)]),
            (vec![true, true], vec![Some(tenant), None]),
            (vec![false, false], vec![None, Some(other)]),
            (
                vec![false, false, true, false],
                vec![None, Some(tenant), Some(tenant), Some(other)],
            ),
        ];

        for (private, tenants) in cases {
            let n = private.len() as u8;
            let reference = {
                let ufs = frame_set(n, &private, &tenants);
                tenant_filter_reference(tenant, &ufs)
            };

            let mut ufs = frame_set(n, &private, &tenants);
            let result = filter_rx_info_by_tenant_id(tenant, &mut ufs);

            if reference.is_empty() {
                assert!(result.is_err(), "empty filtered set must be an error");
            } else {
                assert!(result.is_ok(), "unexpected error: {:?}", result);
                assert_eq!(ids(&ufs), reference, "filtered gateway ids differ");
            }
        }
    }

    #[test]
    fn public_filter_matches_upstream_behaviour() {
        let private = vec![false, true, false, true];
        let tenants: Vec<Option<Uuid>> = vec![None; 4];
        let reference = {
            let ufs = frame_set(4, &private, &tenants);
            public_filter_reference(&ufs)
        };

        let mut ufs = frame_set(4, &private, &tenants);
        assert!(filter_rx_info_by_public_only(&mut ufs).is_ok());
        assert_eq!(ids(&ufs), reference);
    }

    #[test]
    fn malformed_gateway_id_leaves_rx_info_set_untouched() {
        set_test_config();
        let mut ufs = frame_set(2, &[false, false], &[None, None]);
        ufs.rx_info_set[0].gateway_id = "not-an-eui64".into();
        let before = ids(&ufs);

        assert!(filter_rx_info_by_tenant_id(Uuid::new_v4(), &mut ufs).is_err());
        assert_eq!(ids(&ufs), before, "set must not be modified on error");

        assert!(filter_rx_info_by_public_only(&mut ufs).is_err());
        assert_eq!(ids(&ufs), before, "set must not be modified on error");
    }
}
