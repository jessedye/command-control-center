//! Discovery and listener-wide token rates for local AI inference servers.

use crate::snapshot::InferenceSession;
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const MAX_METRICS_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct Process {
    pub pid: u32,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Counter {
    pub harness: String,
    pub endpoint: String,
    pub connect_host: String,
    pub pid: u32,
    pub active: Option<f64>,
    pub queued: Option<f64>,
    pub prompt_total: Option<f64>,
    pub generated_total: Option<f64>,
    pub native_tps: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
struct Harness {
    name: &'static str,
    default_port: u16,
}

#[derive(Default)]
struct Metrics {
    active: Option<f64>,
    queued: Option<f64>,
    prompt_total: Option<f64>,
    generated_total: Option<f64>,
    native_tps: Option<f64>,
}

const RATE_WINDOW_SECS: f64 = 5.0;

#[derive(Clone, Copy)]
struct CounterPoint {
    dt: f64,
    prompt: Option<f64>,
    generated: Option<f64>,
}

#[derive(Default)]
pub struct Baseline {
    history: HashMap<String, VecDeque<CounterPoint>>,
    missed_dt: HashMap<String, f64>,
    last_rates: HashMap<String, (Option<f64>, Option<f64>)>,
}

impl Baseline {
    pub fn new() -> Self {
        Self::default()
    }

    fn window_rate(
        &self,
        key: &str,
        dt: f64,
        current: Option<f64>,
        select: fn(&CounterPoint) -> Option<f64>,
    ) -> Option<f64> {
        let now = current?;
        let history = self.history.get(key)?;
        let mut elapsed = dt + self.missed_dt.get(key).copied().unwrap_or(0.0);
        let mut oldest = None;
        for point in history.iter().rev() {
            oldest = select(point);
            if elapsed >= RATE_WINDOW_SECS {
                break;
            }
            elapsed += point.dt.max(0.0);
        }
        let old = oldest?;
        (elapsed > 0.0 && now >= old).then_some((now - old) / elapsed)
    }

    pub fn rates(&self, dt_secs: f64, counters: &[Counter]) -> Vec<InferenceSession> {
        counters
            .iter()
            .map(|c| {
                let key = counter_key(c);
                let previous = self.last_rates.get(&key).copied().unwrap_or((None, None));
                let missed = self.missed_dt.get(&key).copied().unwrap_or(0.0) + dt_secs;
                let prompt = match c.prompt_total {
                    Some(_) => self.window_rate(&key, dt_secs, c.prompt_total, |p| p.prompt),
                    None if missed <= RATE_WINDOW_SECS => previous.0,
                    None => None,
                };
                let output = match c.generated_total {
                    Some(_) => self.window_rate(&key, dt_secs, c.generated_total, |p| p.generated).or(c.native_tps),
                    None if missed <= RATE_WINDOW_SECS => c.native_tps.or(previous.1),
                    None => c.native_tps,
                };
                InferenceSession {
                    harness: c.harness.clone(),
                    endpoint: c.endpoint.clone(),
                    pid: c.pid,
                    active: c.active,
                    queued: c.queued,
                    prompt_tps: prompt,
                    output_tps: output,
                }
            })
            .collect()
    }

    pub fn commit(&mut self, dt_secs: f64, counters: &[Counter]) {
        let rates = self.rates(dt_secs, counters);
        for (c, row) in counters.iter().zip(rates) {
            let key = counter_key(c);
            if c.prompt_total.is_none() && c.generated_total.is_none() {
                *self.missed_dt.entry(key).or_insert(0.0) += dt_secs.max(0.0);
                continue;
            }
            let dt = dt_secs.max(0.0) + self.missed_dt.remove(&key).unwrap_or(0.0);
            let history = self.history.entry(key.clone()).or_default();
            let reset = history.back().is_some_and(|previous| {
                c.prompt_total.zip(previous.prompt).is_some_and(|(now, old)| now < old)
                    || c.generated_total.zip(previous.generated).is_some_and(|(now, old)| now < old)
            });
            if reset {
                history.clear();
            }
            history.push_back(CounterPoint { dt, prompt: c.prompt_total, generated: c.generated_total });
            while history.len() > 1 {
                let total: f64 = history.iter().map(|point| point.dt.max(0.0)).sum();
                let oldest = history.front().map(|point| point.dt.max(0.0)).unwrap_or(0.0);
                if total - oldest < RATE_WINDOW_SECS {
                    break;
                }
                history.pop_front();
            }
            let previous = self.last_rates.entry(key).or_insert((None, None));
            if c.prompt_total.is_some() {
                previous.0 = row.prompt_tps;
            }
            if c.generated_total.is_some() || c.native_tps.is_some() {
                previous.1 = row.output_tps;
            }
        }
        let live: Vec<String> = counters.iter().map(counter_key).collect();
        self.history.retain(|key, _| live.contains(key));
        self.missed_dt.retain(|key, _| live.contains(key));
        self.last_rates.retain(|key, _| live.contains(key));
    }
}

fn counter_key(c: &Counter) -> String {
    format!("{}|{}", c.harness, c.endpoint)
}

pub fn discover(processes: &[Process]) -> Vec<Counter> {
    let mut found: Vec<Counter> = Vec::new();
    for process in processes {
        let Some(harness) = identify(&process.args) else { continue };
        let port = option_value(&process.args, &["--metrics-port", "--port", "--http-port"])
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or(harness.default_port);
        let advertised = option_value(&process.args, &["--host", "--http-address", "--address"])
            .unwrap_or("127.0.0.1")
            .trim_matches(['[', ']']);
        let host = match advertised {
            "" | "0.0.0.0" | "::" | "*" => "127.0.0.1",
            "localhost" => "127.0.0.1",
            other => other,
        };
        if host.contains('/') || host.chars().any(char::is_whitespace) {
            continue;
        }
        let endpoint = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        if found.iter().any(|c| c.harness == harness.name && c.endpoint == endpoint) {
            continue;
        }
        found.push(Counter {
            harness: harness.name.to_string(),
            endpoint,
            connect_host: host.to_string(),
            pid: process.pid,
            active: None,
            queued: None,
            prompt_total: None,
            generated_total: None,
            native_tps: None,
        });
    }
    found.sort_by(|a, b| a.endpoint.cmp(&b.endpoint).then(a.harness.cmp(&b.harness)));
    found
}

fn identify(args: &[String]) -> Option<Harness> {
    let lower: Vec<String> = args.iter().map(|a| a.to_ascii_lowercase()).collect();
    let has = |needle: &str| lower.iter().any(|a| a.contains(needle));
    let executable = |name: &str| {
        lower.iter().any(|a| {
            a.rsplit('/').next().is_some_and(|base| base == name || base == format!("{name}.exe"))
        })
    };
    let harness = if executable("vllm") && lower.iter().any(|a| a == "serve")
        || has("vllm.entrypoints.openai.api_server")
    {
        Harness { name: "vLLM", default_port: 8000 }
    } else if has("sglang.launch_server") || has("sgl.launch_server") {
        Harness { name: "SGLang", default_port: 30000 }
    } else if executable("text-generation-launcher") {
        Harness { name: "TGI", default_port: 80 }
    } else if executable("trtllm-serve") || has("tensorrt_llm.serve") {
        Harness { name: "TensorRT-LLM", default_port: 8000 }
    } else if executable("tritonserver") {
        Harness { name: "TensorRT-LLM", default_port: 8002 }
    } else if executable("ollama") && lower.iter().any(|a| a == "serve") {
        Harness { name: "Ollama", default_port: 11434 }
    } else if executable("llama-server") {
        Harness { name: "llama.cpp", default_port: 8080 }
    } else if executable("lmdeploy")
        && lower.windows(2).any(|w| w == ["serve", "api_server"])
    {
        Harness { name: "LMDeploy", default_port: 23333 }
    } else {
        return None;
    };
    Some(harness)
}

fn option_value<'a>(args: &'a [String], names: &[&str]) -> Option<&'a str> {
    for (i, arg) in args.iter().enumerate() {
        for name in names {
            if arg == name {
                return args.get(i + 1).map(String::as_str);
            }
            if let Some(value) = arg.strip_prefix(&format!("{name}=")) {
                return Some(value);
            }
        }
    }
    None
}

fn resolve_container_ports(counters: &mut [Counter]) {
    let containerized = counters.iter().any(|c| {
        std::fs::read_to_string(format!("/proc/{}/cgroup", c.pid))
            .is_ok_and(|s| ["docker", "kubepods", "containerd", "libpod"].iter().any(|k| s.contains(k)))
    });
    if !containerized {
        return;
    }
    for runtime in ["docker", "podman"] {
        let Ok((Some(0), ids, _)) = crate::collect::run_bounded(
            runtime,
            &["ps".into(), "-q".into()],
            None,
            Duration::from_secs(2),
        ) else { continue };
        let mut args = vec![
            "inspect".into(),
            "--format".into(),
            "{{.State.Pid}} {{range $port, $bindings := .NetworkSettings.Ports}}{{if $bindings}}{{$port}}={{(index $bindings 0).HostIp}}:{{(index $bindings 0).HostPort}},{{end}}{{end}}".into(),
        ];
        args.extend(String::from_utf8_lossy(&ids).split_whitespace().map(str::to_string));
        if args.len() == 3 {
            return;
        }
        let Ok((Some(0), output, _)) = crate::collect::run_bounded(
            runtime,
            &args,
            None,
            Duration::from_secs(2),
        ) else { continue };
        for line in String::from_utf8_lossy(&output).lines() {
            let mut fields = line.split_whitespace();
            let Some(pid) = fields.next().and_then(|p| p.parse::<u32>().ok()) else { continue };
            let mappings: Vec<&str> = fields.collect();
            for counter in counters.iter_mut().filter(|c| c.pid == pid) {
                let Some(port) = counter.endpoint.rsplit_once(':').and_then(|(_, p)| p.parse::<u16>().ok()) else { continue };
                let prefix = format!("{port}/tcp=");
                for mapping in &mappings {
                    let Some((host, published)) = mapping.strip_prefix(&prefix).and_then(|m| m.trim_end_matches(',').split_once(':')) else { continue };
                    let Ok(published) = published.parse::<u16>() else { continue };
                    let host = match host {
                        "" | "0.0.0.0" | "::" => "127.0.0.1",
                        other => other,
                    };
                    counter.connect_host = host.to_string();
                    counter.endpoint = format!("{host}:{published}");
                }
            }
        }
        return;
    }
}

pub fn sample(processes: &[Process]) -> Vec<Counter> {
    let mut discovered = discover(processes);
    resolve_container_ports(&mut discovered);
    std::thread::scope(|scope| {
        let handles: Vec<_> = discovered
            .into_iter()
            .map(|mut counter| {
                scope.spawn(move || {
                    if let Ok(body) = fetch_metrics(&counter.connect_host, &counter.endpoint) {
                        let metrics = parse_metrics(&body);
                        counter.active = metrics.active;
                        counter.queued = metrics.queued;
                        counter.prompt_total = metrics.prompt_total;
                        counter.generated_total = metrics.generated_total;
                        counter.native_tps = metrics.native_tps;
                    }
                    counter
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    })
}

fn fetch_metrics(host: &str, endpoint: &str) -> Result<String, String> {
    let port = endpoint
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok())
        .ok_or("invalid listener port")?;
    let mut stream = TcpStream::connect_timeout(
        &std::net::SocketAddr::new(
            host.parse().map_err(|_| "only numeric local listen addresses are supported")?,
            port,
        ),
        Duration::from_millis(750),
    )
    .map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(Duration::from_millis(750))).ok();
    stream.set_write_timeout(Some(Duration::from_millis(750))).ok();
    write!(stream, "GET /metrics HTTP/1.1\r\nHost: {endpoint}\r\nAccept: text/plain\r\nConnection: close\r\n\r\n")
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    let read = stream
        .take((MAX_METRICS_BYTES + 1) as u64)
        .read_to_end(&mut bytes);
    if let Err(e) = read {
        if e.kind() != std::io::ErrorKind::ConnectionReset || bytes.is_empty() {
            return Err(e.to_string());
        }
    }
    if bytes.len() > MAX_METRICS_BYTES {
        return Err("metrics response too large".to_string());
    }
    let response = String::from_utf8_lossy(&bytes);
    let (head, body) = response.split_once("\r\n\r\n").ok_or("incomplete HTTP response")?;
    if !head.lines().next().is_some_and(|line| line.contains(" 200 ")) {
        return Err("metrics endpoint did not return HTTP 200".to_string());
    }
    if let Some(length) = head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().ok())?
    }) {
        if body.len() != length {
            return Err("incomplete HTTP body".to_string());
        }
    }
    if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        decode_chunked(body)
    } else {
        Ok(body.to_string())
    }
}

fn decode_chunked(mut body: &str) -> Result<String, String> {
    let mut decoded = String::new();
    loop {
        let (size, rest) = body.split_once("\r\n").ok_or("bad chunk header")?;
        let size = usize::from_str_radix(size.trim().split(';').next().unwrap_or("0"), 16)
            .map_err(|_| "bad chunk size")?;
        if size == 0 {
            return Ok(decoded);
        }
        if rest.len() < size + 2 || decoded.len() + size > MAX_METRICS_BYTES {
            return Err("invalid or oversized chunk".to_string());
        }
        decoded.push_str(&rest[..size]);
        body = &rest[size + 2..];
    }
}

fn parse_metrics(text: &str) -> Metrics {
    let mut metrics = Metrics::default();
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let name_end = line.find(['{', ' ', '\t']);
        let Some(name_end) = name_end else { continue };
        let name = &line[..name_end];
        let value = line[name_end..]
            .split_whitespace()
            .next_back()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite());
        let Some(value) = value else { continue };
        let generated = matches!(
            name,
            "vllm:generation_tokens_total"
                | "vllm:generation_tokens"
                | "sglang:generation_tokens_total"
                | "sglang:generated_tokens_total"
                | "tgi_request_generated_tokens_total"
                | "tgi_request_generated_tokens_sum"
                | "trt_llm:generation_tokens_total"
                | "nv_trt_llm_generation_tokens_total"
                | "ollama_generated_tokens_total"
                | "ollama:generation_tokens_total"
                | "ollama:eval_count_total"
                | "llamacpp:tokens_predicted_total"
                | "llamacpp:tokens_predicted"
                | "lmdeploy:generation_tokens_total"
        );
        let prompt = matches!(
            name,
            "vllm:prompt_tokens_total"
                | "vllm:prompt_tokens"
                | "sglang:prompt_tokens_total"
                | "tgi_request_input_tokens_total"
                | "tgi_request_input_length_sum"
                | "trt_llm:prompt_tokens_total"
                | "nv_trt_llm_prompt_tokens_total"
                | "ollama_prompt_tokens_total"
                | "ollama:prompt_tokens_total"
                | "ollama:prompt_eval_count_total"
                | "llamacpp:prompt_tokens_total"
                | "lmdeploy:prompt_tokens_total"
        );
        let active = matches!(
            name,
            "vllm:num_requests_running"
                | "sglang:num_running_reqs"
                | "tgi_batch_current_size"
                | "ollama_active_requests"
                | "ollama:num_requests_running"
                | "llamacpp:requests_processing"
                | "lmdeploy:num_requests_running"
        );
        let queued = matches!(
            name,
            "vllm:num_requests_waiting"
                | "sglang:num_queue_reqs"
                | "tgi_queue_size"
                | "ollama_pending_requests"
                | "llamacpp:requests_deferred"
                | "lmdeploy:num_requests_waiting"
        );
        add(&mut metrics.generated_total, value, generated);
        add(&mut metrics.prompt_total, value, prompt);
        add(&mut metrics.active, value, active);
        add(&mut metrics.queued, value, queued);
        if name == "sglang:gen_throughput" {
            metrics.native_tps = Some(value.max(0.0));
        }
        if name == "nv_trt_llm_runtime_metrics" {
            if line.contains("metric_type=\"num_requests_running\"") {
                add(&mut metrics.active, value, true);
            } else if line.contains("metric_type=\"num_requests_waiting\"") {
                add(&mut metrics.queued, value, true);
            }
        }
    }
    metrics
}

fn add(slot: &mut Option<f64>, value: f64, matches: bool) {
    if matches {
        *slot = Some(slot.unwrap_or(0.0) + value.max(0.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(args: &[&str]) -> Process {
        Process { pid: 42, args: args.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn discovers_major_harnesses_and_ports() {
        let cases = [
            (vec!["vllm", "serve", "model", "--port=8001"], "vLLM", "127.0.0.1:8001"),
            (vec!["python", "-m", "sglang.launch_server"], "SGLang", "127.0.0.1:30000"),
            (vec!["text-generation-launcher", "--port", "3000"], "TGI", "127.0.0.1:3000"),
            (vec!["trtllm-serve", "model"], "TensorRT-LLM", "127.0.0.1:8000"),
            (vec!["tritonserver", "--model-repository=/models"], "TensorRT-LLM", "127.0.0.1:8002"),
            (vec!["ollama", "serve"], "Ollama", "127.0.0.1:11434"),
            (vec!["llama-server", "--host", "0.0.0.0"], "llama.cpp", "127.0.0.1:8080"),
            (vec!["lmdeploy", "serve", "api_server"], "LMDeploy", "127.0.0.1:23333"),
        ];
        for (args, harness, endpoint) in cases {
            let found = discover(&[process(&args)]);
            assert_eq!(found.len(), 1, "{args:?}");
            assert_eq!(found[0].harness, harness);
            assert_eq!(found[0].endpoint, endpoint);
        }
    }

    #[test]
    fn ignores_workers_and_deduplicates_listeners() {
        let procs = vec![
            process(&["python", "-m", "vllm.entrypoints.openai.api_server", "--port", "8000"]),
            process(&["python", "-m", "vllm.entrypoints.openai.api_server", "--port", "8000"]),
            process(&["python", "-m", "vllm.v1.engine.core"]),
            process(&["python", "train.py"]),
        ];
        assert_eq!(discover(&procs).len(), 1);
    }

    #[test]
    fn parses_labeled_multiprocess_metrics() {
        let parsed = parse_metrics(
            "# HELP x\nvllm:generation_tokens_total{model_name=\"m\",worker=\"0\"} 100\n\
             vllm:generation_tokens_total{model_name=\"m\",worker=\"1\"} 50\n\
             vllm:prompt_tokens_total{model_name=\"m\"} 500\n\
             vllm:num_requests_running{model_name=\"m\"} 2\n\
             vllm:num_requests_waiting{model_name=\"m\"} 3\n",
        );
        assert_eq!(parsed.generated_total, Some(150.0));
        assert_eq!(parsed.prompt_total, Some(500.0));
        assert_eq!(parsed.active, Some(2.0));
        assert_eq!(parsed.queued, Some(3.0));
    }

    #[test]
    fn parses_other_harness_token_metrics() {
        let parsed = parse_metrics(
            "tgi_request_generated_tokens_sum 10\ntgi_request_input_length_sum 20\n\
             llamacpp:tokens_predicted_total 30\nllamacpp:prompt_tokens_total 40\n\
             ollama:eval_count_total 50\nollama:prompt_eval_count_total 60\n\
             trt_llm:generation_tokens_total 70\ntrt_llm:prompt_tokens_total 80\n\
             lmdeploy:generation_tokens_total 90\nlmdeploy:prompt_tokens_total 100\n",
        );
        assert_eq!(parsed.generated_total, Some(250.0));
        assert_eq!(parsed.prompt_total, Some(300.0));
    }

    #[test]
    fn rates_warm_up_and_handle_counter_resets() {
        let mut baseline = Baseline::new();
        let mut counter = Counter {
            harness: "vLLM".into(), endpoint: "127.0.0.1:8000".into(), connect_host: "127.0.0.1".into(),
            pid: 1, active: Some(1.0), queued: None, prompt_total: Some(10.0), generated_total: Some(20.0), native_tps: None,
        };
        assert_eq!(baseline.rates(1.0, &[counter.clone()])[0].output_tps, None);
        baseline.commit(0.0, &[counter.clone()]);
        counter.generated_total = Some(80.0);
        counter.prompt_total = Some(40.0);
        let rows = baseline.rates(2.0, &[counter.clone()]);
        assert_eq!(rows[0].output_tps, Some(30.0));
        assert_eq!(rows[0].prompt_tps, Some(15.0));
        baseline.commit(2.0, &[counter.clone()]);
        counter.generated_total = Some(2.0);
        assert_eq!(baseline.rates(1.0, &[counter])[0].output_tps, None);
    }

    #[test]
    fn rolling_window_smooths_idle_ticks_and_missed_scrapes() {
        let mut baseline = Baseline::new();
        let mut counter = Counter {
            harness: "vLLM".into(), endpoint: "127.0.0.1:8000".into(), connect_host: "127.0.0.1".into(),
            pid: 1, active: Some(1.0), queued: None, prompt_total: Some(0.0), generated_total: Some(0.0), native_tps: None,
        };
        baseline.commit(0.0, &[counter.clone()]);
        counter.generated_total = Some(100.0);
        let active = baseline.rates(1.0, &[counter.clone()]);
        assert_eq!(active[0].output_tps, Some(100.0));
        baseline.commit(1.0, &[counter.clone()]);
        let idle = baseline.rates(1.0, &[counter.clone()]);
        assert_eq!(idle[0].output_tps, Some(50.0));
        baseline.commit(1.0, &[counter.clone()]);
        counter.prompt_total = None;
        counter.generated_total = None;
        assert_eq!(baseline.rates(1.0, &[counter.clone()])[0].output_tps, Some(50.0));
        baseline.commit(1.0, &[counter.clone()]);
        counter.prompt_total = Some(0.0);
        counter.generated_total = Some(100.0);
        assert_eq!(baseline.rates(1.0, &[counter])[0].output_tps, Some(25.0));
    }

    #[test]
    fn rolling_rate_is_independent_of_sample_interval() {
        let mut baseline = Baseline::new();
        let mut counter = Counter {
            harness: "vLLM".into(), endpoint: "127.0.0.1:8000".into(), connect_host: "127.0.0.1".into(),
            pid: 1, active: Some(1.0), queued: None, prompt_total: Some(0.0), generated_total: Some(0.0), native_tps: None,
        };
        baseline.commit(0.0, &[counter.clone()]);
        for _ in 0..12 {
            counter.generated_total = Some(counter.generated_total.unwrap() + 100.0);
            assert_eq!(baseline.rates(2.0, &[counter.clone()])[0].output_tps, Some(50.0));
            baseline.commit(2.0, &[counter.clone()]);
        }
    }

    #[test]
    fn samples_real_http_metrics_and_computes_rates() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            for total in [100, 100, 160] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut chunk = [0u8; 256];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = stream.read(&mut chunk).unwrap();
                    if n == 0 { break; }
                    request.extend_from_slice(&chunk[..n]);
                }
                let body = format!("vllm:generation_tokens_total{{model=\"m\"}} {total}\nvllm:num_requests_running{{model=\"m\"}} 2\n");
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
                stream.flush().unwrap();
            }
        });
        let process = process(&["vllm", "serve", "model", "--port", &port.to_string()]);
        let body = fetch_metrics("127.0.0.1", &format!("127.0.0.1:{port}")).unwrap();
        assert_eq!(parse_metrics(&body).generated_total, Some(100.0));
        let first = sample(&[process.clone()]);
        assert_eq!(first[0].generated_total, Some(100.0));
        assert_eq!(first[0].active, Some(2.0));
        let second = sample(&[process]);
        server.join().unwrap();
        let mut baseline = Baseline::new();
        baseline.commit(0.0, &first);
        let rows = baseline.rates(2.0, &second);
        assert_eq!(rows[0].output_tps, Some(30.0));
    }

    #[test]
    fn native_rate_is_fallback_only() {
        let mut counter = discover(&[process(&["python", "-m", "sglang.launch_server"])])[0].clone();
        counter.native_tps = Some(88.0);
        assert_eq!(Baseline::new().rates(1.0, &[counter])[0].output_tps, Some(88.0));
    }
}
