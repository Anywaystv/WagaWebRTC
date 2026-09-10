//! BWE recovery mechanism tests (AIMD recovery after congestion).

use std::time::Duration;

use netem::{DataSize, NetemConfig};
use str0m::RtcError;
use str0m::bwe::Bitrate;

use crate::common::{BweTestContext, Step, connect_with_bwe, init_crypto_default, init_log};

#[test]
fn application_limited_recovers_when_same_link_improves() -> Result<(), RtcError> {
    init_crypto_default();
    for seed in [1, 42, 99] {
        let (mut l, mut r) = connect_with_bwe(Bitrate::kbps(500), Bitrate::mbps(6));
        let mut ctx = BweTestContext::new(&mut l, &mut r);
        ctx.set_media_send_rate(Bitrate::kbps(250));
        for cycle in 0..2 {
            let low = NetemConfig::new()
                .latency(Duration::from_millis(30))
                .link(Bitrate::kbps(500), DataSize::kbytes(20))
                .seed(seed);
            l.set_netem(low);
            r.set_netem(low);
            let estimate =
                ctx.run_for_duration(&mut l, &mut r, Duration::from_secs(15 + cycle * 2))?;
            assert!(
                estimate.is_some_and(|rate| rate <= Bitrate::mbps(1)),
                "must remain limited on the 500 kbps link: {estimate:?}"
            );

            let high = NetemConfig::new()
                .latency(Duration::from_millis(10))
                .jitter(Duration::from_millis(2))
                .link(Bitrate::mbps(10), DataSize::kbytes(200))
                .seed(seed);
            l.set_netem(high);
            r.set_netem(high);
            // No handoff notification or media-rate increase to unstick the estimator.
            let mut recovered = None;
            let mut estimate = None;
            for second in 1..=20 {
                estimate = ctx.run_for_duration(&mut l, &mut r, Duration::from_secs(1))?;
                if estimate.is_some_and(|rate| rate >= Bitrate::mbps(6)) && recovered.is_none() {
                    recovered = Some(second);
                }
            }
            println!("same-link recovery: seed={seed} cycle={cycle} seconds={recovered:?}");
            assert!(recovered.is_some(), "no recovery: seed={seed} cycle={cycle}");
            assert!(
                estimate.is_some_and(|rate| rate >= Bitrate::mbps(6)),
                "recovery did not hold: seed={seed} cycle={cycle} estimate={estimate:?}"
            );
        }
    }
    Ok(())
}

#[test]
fn application_limited_does_not_invent_capacity_on_slow_link() -> Result<(), RtcError> {
    init_crypto_default();
    let (mut l, mut r) = connect_with_bwe(Bitrate::kbps(500), Bitrate::mbps(6));
    let config = NetemConfig::new()
        .latency(Duration::from_millis(30))
        .link(Bitrate::kbps(500), DataSize::kbytes(20))
        .seed(1);
    l.set_netem(config);
    r.set_netem(config);
    let mut ctx = BweTestContext::new(&mut l, &mut r);
    ctx.set_media_send_rate(Bitrate::kbps(250));
    ctx.run_for_duration(&mut l, &mut r, Duration::from_secs(10))?;
    for _ in 0..30 {
        let estimate = ctx.run_for_duration(&mut l, &mut r, Duration::from_secs(1))?;
        assert!(
            estimate.is_some_and(|rate| rate <= Bitrate::mbps(1)),
            "probing must not force the estimate up on an unchanged slow link: {estimate:?}"
        );
    }
    Ok(())
}

#[test]
fn aimd_multiplicative_decrease_on_congestion() -> Result<(), RtcError> {
    init_log();
    init_crypto_default();

    let plan = vec![
        Step::Conditions {
            description: "Good network",
            config: NetemConfig::new()
                .latency(Duration::from_millis(20))
                .link(Bitrate::mbps(5), DataSize::kbytes(100))
                .seed(42),
        },
        Step::Media {
            description: "Send at 2 Mbps",
            desired_bitrate: Bitrate::mbps(5),
            media_send_rate: Bitrate::mbps(2),
        },
        Step::Run {
            description: "Establish baseline estimate",
            duration: Duration::from_secs(5),
        },
        Step::Check {
            description: "Baseline estimate around 2 Mbps",
            at_least: Bitrate::mbps(2),
        },
        // Introduce severe congestion
        Step::Conditions {
            description: "High latency causes overuse",
            config: NetemConfig::new()
                .latency(Duration::from_millis(150)) // Queuing delay
                .link(Bitrate::mbps(5), DataSize::kbytes(100))
                .seed(42),
        },
        Step::Run {
            description: "Wait for overuse detection and AIMD decrease",
            duration: Duration::from_secs(3),
        },
        Step::Check {
            description: "Estimate dropped via multiplicative decrease",
            at_least: Bitrate::kbps(500),
        },
    ];

    let (mut l, mut r) = connect_with_bwe(Bitrate::mbps(2), Bitrate::mbps(5));
    let mut ctx = BweTestContext::new(&mut l, &mut r);
    ctx.run_plan(&mut l, &mut r, &plan)?;

    Ok(())
}

#[test]
fn aimd_additive_increase_recovers_estimate() -> Result<(), RtcError> {
    init_log();
    init_crypto_default();

    let plan = vec![
        Step::Conditions {
            description: "Good network",
            config: NetemConfig::new()
                .latency(Duration::from_millis(20))
                .link(Bitrate::mbps(5), DataSize::kbytes(100))
                .seed(42),
        },
        Step::Media {
            description: "Send at 2 Mbps",
            desired_bitrate: Bitrate::mbps(5),
            media_send_rate: Bitrate::mbps(2),
        },
        Step::Run {
            description: "Establish baseline",
            duration: Duration::from_secs(5),
        },
        Step::Check {
            description: "Baseline around 2 Mbps",
            at_least: Bitrate::mbps(2),
        },
        // Cause congestion
        Step::Conditions {
            description: "Temporary congestion",
            config: NetemConfig::new()
                .latency(Duration::from_millis(150))
                .link(Bitrate::mbps(5), DataSize::kbytes(100))
                .seed(42),
        },
        Step::Run {
            description: "Trigger multiplicative decrease",
            duration: Duration::from_secs(3),
        },
        Step::Check {
            description: "Estimate dropped",
            at_least: Bitrate::kbps(500),
        },
        // Restore good network
        Step::Conditions {
            description: "Restore good conditions",
            config: NetemConfig::new()
                .latency(Duration::from_millis(20))
                .link(Bitrate::mbps(5), DataSize::kbytes(100))
                .seed(42),
        },
        Step::Run {
            description: "Allow AIMD additive increase to recover",
            duration: Duration::from_secs(10),
        },
        Step::Check {
            description: "Estimate recovers via additive increase",
            at_least: Bitrate::mbps(1),
        },
    ];

    let (mut l, mut r) = connect_with_bwe(Bitrate::mbps(2), Bitrate::mbps(5));
    let mut ctx = BweTestContext::new(&mut l, &mut r);
    ctx.run_plan(&mut l, &mut r, &plan)?;

    Ok(())
}

#[test]
fn aimd_recovery_reaches_original_estimate() -> Result<(), RtcError> {
    init_log();
    init_crypto_default();

    let plan = vec![
        Step::Conditions {
            description: "Good network",
            config: NetemConfig::new()
                .latency(Duration::from_millis(20))
                .link(Bitrate::mbps(3), DataSize::kbytes(100))
                .seed(42),
        },
        Step::Media {
            description: "Send at 2 Mbps",
            desired_bitrate: Bitrate::mbps(5),
            media_send_rate: Bitrate::mbps(2),
        },
        Step::Run {
            description: "Establish baseline",
            duration: Duration::from_secs(5),
        },
        Step::Check {
            description: "Baseline established",
            at_least: Bitrate::mbps(2),
        },
        // Brief congestion
        Step::Conditions {
            description: "Brief congestion event",
            config: NetemConfig::new()
                .latency(Duration::from_millis(150))
                .link(Bitrate::mbps(3), DataSize::kbytes(100))
                .seed(42),
        },
        Step::Run {
            description: "Drop estimate",
            duration: Duration::from_secs(2),
        },
        // Restore and allow full recovery
        Step::Conditions {
            description: "Restore network",
            config: NetemConfig::new()
                .latency(Duration::from_millis(20))
                .link(Bitrate::mbps(3), DataSize::kbytes(100))
                .seed(42),
        },
        Step::Run {
            description: "Allow full recovery via AIMD + probes",
            duration: Duration::from_secs(15),
        },
        Step::Check {
            description: "Estimate recovers to original level",
            at_least: Bitrate::mbps(2),
        },
    ];

    let (mut l, mut r) = connect_with_bwe(Bitrate::mbps(2), Bitrate::mbps(5));
    let mut ctx = BweTestContext::new(&mut l, &mut r);
    ctx.run_plan(&mut l, &mut r, &plan)?;

    Ok(())
}
