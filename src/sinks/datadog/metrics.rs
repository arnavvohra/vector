use crate::{
    dns::Resolver,
    event::{
        metric::{Metric, MetricKind, MetricValue, StatisticKind},
        Event,
    },
    sinks::util::{
        http::{BatchedHttpSink, HttpClient, HttpSink},
        service2::TowerRequestConfig,
        BatchEventsConfig, MetricBuffer,
    },
    statistic::Summary,
    topology::config::{DataType, SinkConfig, SinkContext, SinkDescription},
};
use chrono::{DateTime, Utc};
use futures::{FutureExt, TryFutureExt};
use futures01::Sink;
use http::{uri::InvalidUri, Request, StatusCode, Uri};
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use snafu::{ResultExt, Snafu};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, Ordering::SeqCst};

#[derive(Debug, Snafu)]
enum BuildError {
    #[snafu(display("Invalid host {:?}: {:?}", host, source))]
    InvalidHost { host: String, source: InvalidUri },
}

#[derive(Clone)]
struct DatadogState {
    last_sent_timestamp: i64,
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct DatadogConfig {
    pub namespace: String,
    #[serde(default = "default_host")]
    pub host: String,
    pub api_key: String,
    #[serde(default)]
    pub batch: BatchEventsConfig,
    #[serde(default)]
    pub request: TowerRequestConfig,
}

struct DatadogSink {
    config: DatadogConfig,
    last_sent_timestamp: AtomicI64,
    uri: Uri,
}

lazy_static! {
    static ref REQUEST_DEFAULTS: TowerRequestConfig = TowerRequestConfig {
        retry_attempts: Some(5),
        ..Default::default()
    };
}

// https://docs.datadoghq.com/api/?lang=bash#post-timeseries-points
#[derive(Debug, Clone, PartialEq, Serialize)]
struct DatadogRequest {
    series: Vec<DatadogMetric>,
}

pub fn default_host() -> String {
    String::from("https://api.datadoghq.com")
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct DatadogMetric {
    metric: String,
    r#type: DatadogMetricType,
    interval: Option<i64>,
    points: Vec<DatadogPoint>,
    tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DatadogMetricType {
    Gauge,
    Count,
    Rate,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct DatadogPoint(i64, f64);

inventory::submit! {
    SinkDescription::new::<DatadogConfig>("datadog_metrics")
}

#[typetag::serde(name = "datadog_metrics")]
impl SinkConfig for DatadogConfig {
    fn build(&self, cx: SinkContext) -> crate::Result<(super::RouterSink, super::Healthcheck)> {
        let healthcheck = healthcheck(self.clone(), cx.resolver()).boxed().compat();

        let batch = self.batch.unwrap_or(20, 1);
        let request = self.request.unwrap_with(&REQUEST_DEFAULTS);

        let uri = build_uri(&self.host)?;
        let timestamp = Utc::now().timestamp();

        let sink = DatadogSink {
            config: self.clone(),
            uri,
            last_sent_timestamp: AtomicI64::new(timestamp),
        };

        let sink = BatchedHttpSink::new(sink, MetricBuffer::new(), request, batch, None, &cx)
            .sink_map_err(|e| error!("Fatal datadog error: {}", e));

        Ok((Box::new(sink), Box::new(healthcheck)))
    }

    fn input_type(&self) -> DataType {
        DataType::Metric
    }

    fn sink_type(&self) -> &'static str {
        "datadog_metrics"
    }
}

#[async_trait::async_trait]
impl HttpSink for DatadogSink {
    type Input = Event;
    type Output = Vec<Metric>;

    fn encode_event(&self, event: Event) -> Option<Self::Input> {
        Some(event)
    }

    async fn build_request(&self, events: Self::Output) -> crate::Result<Request<Vec<u8>>> {
        let now = Utc::now().timestamp();
        let interval = now - self.last_sent_timestamp.load(SeqCst);
        self.last_sent_timestamp.store(now, SeqCst);

        let input = encode_events(events, interval, &self.config.namespace);
        let body = serde_json::to_vec(&input).unwrap();

        Request::post(self.uri.clone())
            .header("Content-Type", "application/json")
            .header("DD-API-KEY", self.config.api_key.clone())
            .body(body)
            .map_err(Into::into)
    }
}

fn build_uri(host: &str) -> crate::Result<Uri> {
    let uri = format!("{}/api/v1/series", host)
        .parse::<Uri>()
        .context(super::UriParseError)?;

    Ok(uri)
}

async fn healthcheck(config: DatadogConfig, resolver: Resolver) -> crate::Result<()> {
    let uri = format!("{}/api/v1/validate", config.host)
        .parse::<Uri>()
        .context(super::UriParseError)?;

    let request = Request::get(uri)
        .header("DD-API-KEY", config.api_key)
        .body(hyper::Body::empty())
        .unwrap();

    let mut client = HttpClient::new(resolver, None)?;
    let response = client.send(request).await?;

    match response.status() {
        StatusCode::OK => Ok(()),
        other => Err(super::HealthcheckError::UnexpectedStatus { status: other }.into()),
    }
}

fn encode_tags(tags: BTreeMap<String, String>) -> Vec<String> {
    let mut pairs: Vec<_> = tags
        .iter()
        .map(|(name, value)| format!("{}:{}", name, value))
        .collect();
    pairs.sort();
    pairs
}

fn encode_timestamp(timestamp: Option<DateTime<Utc>>) -> i64 {
    if let Some(ts) = timestamp {
        ts.timestamp()
    } else {
        Utc::now().timestamp()
    }
}

fn encode_namespace(namespace: &str, name: &str) -> String {
    if !namespace.is_empty() {
        format!("{}.{}", namespace, name)
    } else {
        name.to_string()
    }
}

fn encode_events(events: Vec<Metric>, interval: i64, namespace: &str) -> DatadogRequest {
    let series = events
        .into_iter()
        .filter_map(|event| {
            let fullname = encode_namespace(namespace, &event.name);
            let ts = encode_timestamp(event.timestamp);
            let tags = event.tags.clone().map(encode_tags);
            match event.kind {
                MetricKind::Incremental => match event.value {
                    MetricValue::Counter { value } => Some(vec![DatadogMetric {
                        metric: fullname,
                        r#type: DatadogMetricType::Count,
                        interval: Some(interval),
                        points: vec![DatadogPoint(ts, value)],
                        tags,
                    }]),
                    MetricValue::Samples {
                        values,
                        sample_rates,
                        statistic,
                    } => {
                        Summary::new(&values, &sample_rates, statistic).map(|s| {
                            let metric = |metric, r#type, value| DatadogMetric {
                                metric,
                                r#type,
                                interval: Some(interval),
                                points: vec![DatadogPoint(ts, value)],
                                tags: tags.clone(),
                            };
                            match statistic {
                                // https://docs.datadoghq.com/developers/metrics/metrics_type/?tab=histogram#metric-type-definition
                                StatisticKind::Histogram => {
                                    let mut result = vec![
                                        metric(
                                            format!("{}.min", &fullname),
                                            DatadogMetricType::Gauge,
                                            s.min,
                                        ),
                                        metric(
                                            format!("{}.avg", &fullname),
                                            DatadogMetricType::Gauge,
                                            s.avg,
                                        ),
                                        metric(
                                            format!("{}.count", &fullname),
                                            DatadogMetricType::Rate,
                                            s.count,
                                        ),
                                        metric(
                                            format!("{}.median", &fullname),
                                            DatadogMetricType::Gauge,
                                            s.median,
                                        ),
                                        metric(
                                            format!("{}.max", &fullname),
                                            DatadogMetricType::Gauge,
                                            s.max,
                                        ),
                                    ];

                                    for (q, v) in s.quantiles {
                                        result.push(metric(
                                            format!(
                                                "{}.{}percentile",
                                                &fullname,
                                                (q * 100.0) as u32
                                            ),
                                            DatadogMetricType::Gauge,
                                            v,
                                        ))
                                    }

                                    result
                                }
                                // https://docs.datadoghq.com/developers/metrics/types/?tab=distribution#definition
                                StatisticKind::Distribution => {
                                    let mut result = vec![
                                        metric(
                                            format!("min:{}", &fullname),
                                            DatadogMetricType::Gauge,
                                            s.min,
                                        ),
                                        metric(
                                            format!("avg:{}", &fullname),
                                            DatadogMetricType::Gauge,
                                            s.avg,
                                        ),
                                        metric(
                                            format!("count:{}", &fullname),
                                            DatadogMetricType::Count,
                                            s.count,
                                        ),
                                        metric(
                                            format!("max:{}", &fullname),
                                            DatadogMetricType::Gauge,
                                            s.max,
                                        ),
                                        metric(
                                            format!("sum:{}", &fullname),
                                            DatadogMetricType::Count,
                                            s.sum,
                                        ),
                                    ];

                                    for (q, v) in s.quantiles {
                                        result.push(metric(
                                            format!("p{}:{}", (q * 100.0) as u32, fullname),
                                            DatadogMetricType::Gauge,
                                            v,
                                        ))
                                    }

                                    result
                                }
                            }
                        })
                    }
                    MetricValue::Set { values } => Some(vec![DatadogMetric {
                        metric: fullname,
                        r#type: DatadogMetricType::Gauge,
                        interval: None,
                        points: vec![DatadogPoint(ts, values.len() as f64)],
                        tags,
                    }]),
                    _ => None,
                },
                MetricKind::Absolute => match event.value {
                    MetricValue::Gauge { value } => Some(vec![DatadogMetric {
                        metric: fullname,
                        r#type: DatadogMetricType::Gauge,
                        interval: None,
                        points: vec![DatadogPoint(ts, value)],
                        tags,
                    }]),
                    _ => None,
                },
            }
        })
        .flatten()
        .collect::<Vec<_>>();

    DatadogRequest { series }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::metric::{Metric, MetricKind, MetricValue, StatisticKind};
    use crate::sinks::util::{http::HttpSink, test::load_sink};
    use crate::statistic::Summary;
    use crate::test_util::runtime;
    use chrono::offset::TimeZone;
    use chrono::Utc;
    use http::{Method, Uri};
    use pretty_assertions::assert_eq;
    use std::sync::atomic::AtomicI64;

    fn ts() -> DateTime<Utc> {
        Utc.ymd(2018, 11, 14).and_hms_nano(8, 9, 10, 11)
    }

    fn tags() -> BTreeMap<String, String> {
        vec![
            ("normal_tag".to_owned(), "value".to_owned()),
            ("true_tag".to_owned(), "true".to_owned()),
            ("empty_tag".to_owned(), "".to_owned()),
        ]
        .into_iter()
        .collect()
    }

    #[test]
    fn test_request() {
        let (sink, _, _) = load_sink::<DatadogConfig>(
            r#"
            namespace = "test"
            api_key = "test"
        "#,
        )
        .unwrap();

        let timestamp = Utc::now().timestamp();
        let sink = DatadogSink {
            config: sink,
            uri: build_uri(&default_host()).unwrap(),
            last_sent_timestamp: AtomicI64::new(timestamp),
        };

        let events = vec![
            Metric {
                name: "total".into(),
                timestamp: None,
                tags: None,
                kind: MetricKind::Incremental,
                value: MetricValue::Counter { value: 1.5 },
            },
            Metric {
                name: "check".into(),
                timestamp: Some(ts()),
                tags: Some(tags()),
                kind: MetricKind::Incremental,
                value: MetricValue::Counter { value: 1.0 },
            },
            Metric {
                name: "unsupported".into(),
                timestamp: Some(ts()),
                tags: Some(tags()),
                kind: MetricKind::Absolute,
                value: MetricValue::Counter { value: 1.0 },
            },
        ];

        let mut rt = runtime();
        let req = rt
            .block_on_std(async move { sink.build_request(events).await })
            .unwrap();

        assert_eq!(req.method(), Method::POST);
        assert_eq!(
            req.uri(),
            &Uri::from_static("https://api.datadoghq.com/api/v1/series")
        );
    }

    #[test]
    fn test_encode_tags() {
        assert_eq!(
            encode_tags(tags()),
            vec!["empty_tag:", "normal_tag:value", "true_tag:true"]
        );
    }

    #[test]
    fn test_encode_timestamp() {
        assert_eq!(encode_timestamp(None), Utc::now().timestamp());
        assert_eq!(encode_timestamp(Some(ts())), 1542182950);
    }

    #[test]
    fn encode_counter() {
        let now = Utc::now().timestamp();
        let interval = 60;
        let events = vec![
            Metric {
                name: "total".into(),
                timestamp: None,
                tags: None,
                kind: MetricKind::Incremental,
                value: MetricValue::Counter { value: 1.5 },
            },
            Metric {
                name: "check".into(),
                timestamp: Some(ts()),
                tags: Some(tags()),
                kind: MetricKind::Incremental,
                value: MetricValue::Counter { value: 1.0 },
            },
            Metric {
                name: "unsupported".into(),
                timestamp: Some(ts()),
                tags: Some(tags()),
                kind: MetricKind::Absolute,
                value: MetricValue::Counter { value: 1.0 },
            },
        ];
        let input = encode_events(events, interval, "ns");
        let json = serde_json::to_string(&input).unwrap();

        assert_eq!(
            json,
            format!("{{\"series\":[{{\"metric\":\"ns.total\",\"type\":\"count\",\"interval\":60,\"points\":[[{},1.5]],\"tags\":null}},{{\"metric\":\"ns.check\",\"type\":\"count\",\"interval\":60,\"points\":[[1542182950,1.0]],\"tags\":[\"empty_tag:\",\"normal_tag:value\",\"true_tag:true\"]}}]}}", now)
        );
    }

    #[test]
    fn encode_gauge() {
        let events = vec![
            Metric {
                name: "unsupported".into(),
                timestamp: Some(ts()),
                tags: None,
                kind: MetricKind::Incremental,
                value: MetricValue::Gauge { value: 0.1 },
            },
            Metric {
                name: "volume".into(),
                timestamp: Some(ts()),
                tags: None,
                kind: MetricKind::Absolute,
                value: MetricValue::Gauge { value: -1.1 },
            },
        ];
        let input = encode_events(events, 60, "");
        let json = serde_json::to_string(&input).unwrap();

        assert_eq!(
            json,
            r#"{"series":[{"metric":"volume","type":"gauge","interval":null,"points":[[1542182950,-1.1]],"tags":null}]}"#
        );
    }

    #[test]
    fn encode_set() {
        let events = vec![Metric {
            name: "users".into(),
            timestamp: Some(ts()),
            tags: None,
            kind: MetricKind::Incremental,
            value: MetricValue::Set {
                values: vec!["alice".into(), "bob".into()].into_iter().collect(),
            },
        }];
        let input = encode_events(events, 60, "");
        let json = serde_json::to_string(&input).unwrap();

        assert_eq!(
            json,
            r#"{"series":[{"metric":"users","type":"gauge","interval":null,"points":[[1542182950,2.0]],"tags":null}]}"#
        );
    }

    #[test]
    fn test_dense_stats() {
        // https://github.com/DataDog/dd-agent/blob/master/tests/core/test_histogram.py
        let values = (0..20).into_iter().map(f64::from).collect::<Vec<_>>();
        let counts = vec![1; 20];

        assert_eq!(
            Summary::new(&values, &counts, StatisticKind::Histogram),
            Some(Summary {
                min: 0.0,
                max: 19.0,
                median: 9.0,
                avg: 9.5,
                sum: 190.0,
                count: 20.0,
                quantiles: vec![(0.95, 18.0)],
            })
        );
    }

    #[test]
    fn test_sparse_stats() {
        let values = (1..5).into_iter().map(f64::from).collect::<Vec<_>>();
        let counts = (1..5).into_iter().collect::<Vec<_>>();

        assert_eq!(
            Summary::new(&values, &counts, StatisticKind::Histogram),
            Some(Summary {
                min: 1.0,
                max: 4.0,
                median: 3.0,
                avg: 3.0,
                sum: 30.0,
                count: 10.0,
                quantiles: vec![(0.95, 4.0)],
            })
        );
    }

    #[test]
    fn test_single_value_stats() {
        let values = vec![10.0];
        let counts = vec![1];

        assert_eq!(
            Summary::new(&values, &counts, StatisticKind::Histogram),
            Some(Summary {
                min: 10.0,
                max: 10.0,
                median: 10.0,
                avg: 10.0,
                sum: 10.0,
                count: 1.0,
                quantiles: vec![(0.95, 10.0)],
            })
        );
    }
    #[test]
    fn test_nan_stats() {
        let values = vec![1.0, std::f64::NAN];
        let counts = vec![1, 1];
        assert!(Summary::new(&values, &counts, StatisticKind::Histogram).is_some());
    }

    #[test]
    fn test_unequal_stats() {
        let values = vec![1.0];
        let counts = vec![1, 2, 3];
        assert!(Summary::new(&values, &counts, StatisticKind::Histogram).is_none());
    }

    #[test]
    fn test_empty_stats() {
        let values = vec![];
        let counts = vec![];
        assert!(Summary::new(&values, &counts, StatisticKind::Histogram).is_none());
    }

    #[test]
    fn test_zero_counts_stats() {
        let values = vec![1.0, 2.0];
        let counts = vec![0, 0];
        assert!(Summary::new(&values, &counts, StatisticKind::Histogram).is_none());
    }

    #[test]
    fn encode_histogram() {
        // https://docs.datadoghq.com/developers/metrics/metrics_type/?tab=histogram#metric-type-definition
        let events = vec![Metric {
            name: "requests".into(),
            timestamp: Some(ts()),
            tags: None,
            kind: MetricKind::Incremental,
            value: MetricValue::Samples {
                values: vec![1.0, 2.0, 3.0],
                sample_rates: vec![3, 3, 2],
                statistic: StatisticKind::Histogram,
            },
        }];
        let input = encode_events(events, 60, "");
        let json = serde_json::to_string(&input).unwrap();

        assert_eq!(
            json,
            r#"{"series":[{"metric":"requests.min","type":"gauge","interval":60,"points":[[1542182950,1.0]],"tags":null},{"metric":"requests.avg","type":"gauge","interval":60,"points":[[1542182950,1.875]],"tags":null},{"metric":"requests.count","type":"rate","interval":60,"points":[[1542182950,8.0]],"tags":null},{"metric":"requests.median","type":"gauge","interval":60,"points":[[1542182950,2.0]],"tags":null},{"metric":"requests.max","type":"gauge","interval":60,"points":[[1542182950,3.0]],"tags":null},{"metric":"requests.95percentile","type":"gauge","interval":60,"points":[[1542182950,3.0]],"tags":null}]}"#
        );
    }

    #[test]
    fn encode_distribution() {
        // https://docs.datadoghq.com/developers/metrics/types/?tab=distribution#definition
        let events = vec![Metric {
            name: "requests".into(),
            timestamp: Some(ts()),
            tags: None,
            kind: MetricKind::Incremental,
            value: MetricValue::Samples {
                values: vec![1.0, 2.0, 3.0],
                sample_rates: vec![3, 3, 2],
                statistic: StatisticKind::Distribution,
            },
        }];
        let input = encode_events(events, 60, "");
        let json = serde_json::to_string(&input).unwrap();

        assert_eq!(
            json,
            r#"{"series":[{"metric":"min:requests","type":"gauge","interval":60,"points":[[1542182950,1.0]],"tags":null},{"metric":"avg:requests","type":"gauge","interval":60,"points":[[1542182950,1.875]],"tags":null},{"metric":"count:requests","type":"count","interval":60,"points":[[1542182950,8.0]],"tags":null},{"metric":"max:requests","type":"gauge","interval":60,"points":[[1542182950,3.0]],"tags":null},{"metric":"sum:requests","type":"count","interval":60,"points":[[1542182950,15.0]],"tags":null},{"metric":"p50:requests","type":"gauge","interval":60,"points":[[1542182950,2.0]],"tags":null},{"metric":"p75:requests","type":"gauge","interval":60,"points":[[1542182950,2.0]],"tags":null},{"metric":"p90:requests","type":"gauge","interval":60,"points":[[1542182950,3.0]],"tags":null},{"metric":"p95:requests","type":"gauge","interval":60,"points":[[1542182950,3.0]],"tags":null},{"metric":"p99:requests","type":"gauge","interval":60,"points":[[1542182950,3.0]],"tags":null}]}"#
        );
    }
}
