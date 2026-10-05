use crate::{Cause, Endpoint, Event, Observation, Operation, Report, ReportBundle, Stage};
use std::fmt::Write;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Confidence {
    Observed,
    Possible,
}
#[derive(Clone, Debug)]
pub struct Finding {
    pub endpoint: Endpoint,
    pub confidence: Confidence,
    pub message: String,
    pub evidence: Vec<u64>,
    pub next_check: &'static str,
}
#[derive(Clone, Debug, Default)]
pub struct Explanation {
    pub findings: Vec<Finding>,
    pub limitations: Vec<String>,
}

pub fn explain(bundle: &ReportBundle) -> Explanation {
    let mut result = Explanation::default();
    if let Err(error) = bundle.validate() {
        result.limitations.push(format!("Report rejected: {error}"));
        return result;
    }
    for report in std::iter::once(&bundle.local).chain(bundle.peer.iter()) {
        analyze(report, &mut result);
        if report.endpoint == Endpoint::Receiver
            && !report.events.iter().any(|event| {
                matches!(
                    event.observation,
                    Observation::Stage {
                        stage: Stage::Presentation,
                        ..
                    }
                )
            })
        {
            result.limitations.push("Receiver presentation measurements are missing; successful decoding does not prove smooth display.".into());
        }
    }
    if bundle.peer.is_none() {
        result.limitations.push("Peer evidence is missing. Collect its matching session report after reconnecting, or import an export.".into());
    } else {
        result.limitations.push("Endpoint clocks are independent. Timelines are not aligned; peer observations are peer-reported evidence.".into());
    }
    result.limitations.push("No OS Wi-Fi, interface-transition or thermal evidence was collected by this version. Transport observations cannot identify a faulty router or radio.".into());
    if result.findings.is_empty() {
        result.limitations.push("No supported failure or lag pattern was found in the retained evidence. This does not prove that the session was smooth.".into());
    }
    result
}

fn analyze(report: &Report, result: &mut Explanation) {
    let mut finding = |event: &Event, confidence, message, next_check| {
        result.findings.push(Finding {
            endpoint: report.endpoint,
            confidence,
            message,
            evidence: vec![event.sequence],
            next_check,
        });
    };
    if let Some(event) = &report.first_failure
        && let Observation::Failure { operation, cause } = event.observation
    {
        let action = match operation {
            Operation::SessionRead => "waiting for an application-control reply/request",
            Operation::SessionWrite => "sending an application-control record",
            Operation::Control => "processing the hoist control stream",
            Operation::Media => "processing the video stream",
            Operation::Connection => "maintaining the transport connection",
            Operation::Encode => "encoding video",
            Operation::Decode => "decoding video",
            Operation::Presentation => "presenting video",
            Operation::Shutdown => "closing the session",
        };
        let reason = match cause {
            Cause::Timeout => "A local deadline expired",
            Cause::PeerClosed => "The peer closed the connection",
            Cause::TransportLost => "The transport connection was lost",
            Cause::ProtocolOrIo => "A protocol or I/O operation failed",
            Cause::Codec => "The codec reported a failure",
            Cause::LocalShutdown => "The local endpoint requested shutdown",
        };
        finding(
            event,
            Confidence::Observed,
            format!(
                "{reason} while {action}. This is the first recorded failure, not necessarily the underlying cause."
            ),
            "Compare transport and pipeline progress immediately before this event.",
        );
    }
    let mut network_baseline = None;
    let mut reported_network = false;
    let mut reported_stages = Vec::new();
    for event in &report.events {
        match event.observation {
            Observation::Network(sample) => {
                if let Some(previous) = network_baseline {
                    let previous: crate::NetworkSample = previous;
                    if previous.epoch == sample.epoch && !reported_network {
                        let increased_rtt = sample.rtt_us >= previous.rtt_us.saturating_mul(2)
                            && sample.rtt_us.saturating_sub(previous.rtt_us) >= 20_000;
                        let increased_loss = sample.lost_packets > previous.lost_packets;
                        if increased_rtt || increased_loss {
                            finding(
                                event,
                                Confidence::Possible,
                                format!(
                                    "Transport delay/loss increased: RTT {:.1} to {:.1} ms; {} additional lost packets. This can contribute to lag, but does not locate the fault.",
                                    previous.rtt_us as f64 / 1000.0,
                                    sample.rtt_us as f64 / 1000.0,
                                    sample.lost_packets.saturating_sub(previous.lost_packets)
                                ),
                                "Compare both endpoints and an independent latency probe on the same network path.",
                            );
                            reported_network = true;
                        }
                    }
                }
                if !reported_network && sample.rtt_us >= 100_000 && sample.received_bytes > 0 {
                    finding(
                        event,
                        Confidence::Possible,
                        format!(
                            "Transport RTT estimate was {:.1} ms. A consistently high RTT can make interaction slow even without packet loss; it does not measure one-way video latency.",
                            sample.rtt_us as f64 / 1000.0
                        ),
                        "Check direct versus relay routing and compare an independent latency probe.",
                    );
                    reported_network = true;
                }
                network_baseline = Some(sample);
            }
            Observation::Stage { stage, sample } if !reported_stages.contains(&stage) => {
                if sample.pending > 0 && sample.oldest_pending_us >= 100_000 {
                    finding(
                        event,
                        Confidence::Observed,
                        format!(
                            "{stage:?} had {} pending items; reported pending age was {:.1} ms.",
                            sample.pending,
                            sample.oldest_pending_us as f64 / 1000.0
                        ),
                        "Compare upstream progress with this stage. Pending receive work can mean missing media; it does not prove decoder saturation.",
                    );
                    reported_stages.push(stage);
                } else if sample.completed > 0 && sample.work_max_us >= 50_000 {
                    finding(
                        event,
                        Confidence::Possible,
                        format!(
                            "A {stage:?} operation took {:.1} ms, potentially contributing to uneven frame delivery.",
                            sample.work_max_us as f64 / 1000.0
                        ),
                        "Inspect repeated slow operations and local scheduling. Stage durations may overlap and include waits.",
                    );
                    reported_stages.push(stage);
                } else if stage == Stage::Presentation
                    && (sample.superseded > 0 || sample.stale > 0)
                {
                    finding(
                        event,
                        Confidence::Observed,
                        format!(
                            "Presentation replaced {} queued snapshots and skipped {} stale snapshots in this interval. These are local discards, not network packet loss.",
                            sample.superseded, sample.stale
                        ),
                        "Compare arrival bursts and display pacing; replacement alone does not identify a slow GPU.",
                    );
                    reported_stages.push(stage);
                }
            }
            _ => {}
        }
    }
    if report.overwritten_events > 0 {
        result.limitations.push(format!(
            "{:?}: {} older events were overwritten by the bounded recorder.",
            report.endpoint, report.overwritten_events
        ));
    }
}

impl Explanation {
    pub fn render(&self, verbose: bool) -> String {
        let mut text = String::new();
        for finding in &self.findings {
            let _ = writeln!(
                text,
                "{:?} / {:?}: {}",
                finding.endpoint, finding.confidence, finding.message
            );
            if verbose {
                let _ = writeln!(
                    text,
                    "  Evidence events: {:?}\n  Next check: {}",
                    finding.evidence, finding.next_check
                );
            }
        }
        for limitation in &self.limitations {
            let _ = writeln!(text, "Note: {limitation}");
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NetworkSample, Recorder, SessionId, StageSample};
    fn recorder() -> Recorder {
        Recorder::new(SessionId([2; 16]), Endpoint::Receiver)
    }
    #[test]
    fn timeout_is_explained_without_inventing_a_network_or_decoder_cause() {
        let recorder = recorder();
        recorder.record(Observation::Stage {
            stage: Stage::Receive,
            sample: StageSample {
                pending: 10,
                oldest_pending_us: 5_000_000,
                ..Default::default()
            },
        });
        recorder.record(Observation::Failure {
            operation: Operation::SessionRead,
            cause: Cause::Timeout,
        });
        let explanation = explain(&ReportBundle {
            local: recorder.snapshot().expect("report"),
            peer: None,
        });
        assert_eq!(explanation.findings.len(), 2);
        assert!(
            explanation
                .findings
                .iter()
                .all(|f| f.confidence == Confidence::Observed)
        );
        assert!(
            explanation
                .render(true)
                .contains("Peer evidence is missing")
        );
    }
    #[test]
    fn idle_sources_and_new_path_epochs_do_not_generate_lag_findings() {
        let recorder = recorder();
        recorder.record(Observation::Stage {
            stage: Stage::Receive,
            sample: StageSample::default(),
        });
        recorder.record(Observation::Network(NetworkSample {
            epoch: 1,
            rtt_us: 1000,
            ..Default::default()
        }));
        recorder.record(Observation::Network(NetworkSample {
            epoch: 2,
            rtt_us: 90000,
            lost_packets: 20,
            ..Default::default()
        }));
        assert!(
            explain(&ReportBundle {
                local: recorder.snapshot().expect("report"),
                peer: None
            })
            .findings
            .is_empty()
        );
        recorder.record(Observation::Network(NetworkSample {
            epoch: 2,
            rtt_us: 190000,
            lost_packets: 21,
            ..Default::default()
        }));
        assert_eq!(
            explain(&ReportBundle {
                local: recorder.snapshot().expect("report"),
                peer: None
            })
            .findings[0]
                .confidence,
            Confidence::Possible
        );
    }
}
