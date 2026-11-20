# Break It, Then Trace It

Observability for WebAssembly backends on wasmCloud v2 (SREday demo).

```
kubectl apply ──► kind (Kubernetes) ──► runtime-operator ──(scheduler NATS)──► wash host (host group "sreday")
                                                                                ├─ pulls router + app-a/b/c from the OCI registry
                                                                                ├─ wasi:keyvalue → data NATS (JetStream KV)
                                                                                └─ OTel → collector → Tempo / Prometheus / Loki → Grafana
```

## Prerequisites

- Rust (stable) with the `wasm32-wasip2` target
- wash CLI **v2.12.2**: wasmCloud build and runtime tool
- Docker: for kind, the registry, the data NATS and the observability stack
- kind: local Kubernetes
- kubectl
- helm
- oras: OCI registry client (`wash oci push` is not used)
- nats CLI: creates the keyvalue buckets and sets the chaos feature flags
- k6: steady load for the dashboard (https://grafana.com/docs/k6/latest/set-up/install-k6/)

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
rustup target add wasm32-wasip2

curl -fsSL https://wasmcloud.com/sh | bash          # or: brew install wasmcloud/wasmcloud/wash
wash --version                                      # wash 2.12.2
```

kind: https://kind.sigs.k8s.io/docs/user/quick-start/#installation ·
helm: https://helm.sh/docs/intro/install/ ·
oras: https://oras.land/docs/installation ·
nats CLI: https://github.com/nats-io/natscli/releases

## Step 1: Build the components

```bash
cd components

(cd router        && wash build --skip-fetch)
(cd tenants/app-a && wash build --skip-fetch)
(cd tenants/app-b && wash build --skip-fetch)
(cd tenants/app-c && wash build --skip-fetch)

cd ..
```

Each component has its own `.wash/config.yaml` with the build command and output path.
`--skip-fetch` because the WIT dependencies are already vendored in each `wit/deps/`.

| Component | Output (`components/target/wasm32-wasip2/release/`) |
|---|---|
| router | `router.wasm` |
| app-a | `app_a.wasm` |
| app-b | `app_b.wasm` |
| app-c | `app_c.wasm` |

## Step 2: Start the observability stack and the vendor mock

```bash
docker compose up -d
```

Starts the OTel Collector (`:4317`), Tempo, Prometheus, Loki, Grafana (`:3000`),
a Blackbox Exporter that probes the wasmCloud host's `/healthz` from outside (availability),
the mock vendor API, and toxiproxy in front of it (`:8666` for app-a, `:8667` for app-c).

## Step 3: Create the kind cluster

```bash
kind create cluster --name sreday
kubectl config use-context kind-sreday
kubectl get nodes
```

## Step 4: Install the wasmCloud operator via Helm

```bash
helm install wasmcloud \
  oci://ghcr.io/wasmcloud/charts/runtime-operator \
  --version 2.12.2 \
  --namespace wasmcloud \
  --create-namespace \
  --set global.tls.enabled=false \
  --set gateway.enabled=false \
  --set-json 'runtime.hostGroups=[]'

kubectl rollout status deployment/runtime-operator -n wasmcloud
kubectl rollout status deployment/nats -n wasmcloud
kubectl get pods -n wasmcloud
```

- The chart installs the operator, the wasmCloud CRDs and an in-cluster NATS (the
  scheduler / control plane).
- `gateway.enabled=false`: the runtime-gateway is deprecated.
- `runtime.hostGroups=[]`: no in-cluster hosts. The host runs on this machine (Step 8).
- In chart 2.12.2 the deployment is named `runtime-operator`, not `wasmcloud-runtime-operator`.

## Step 5: Port-forward the scheduler NATS

Keep this running in its own terminal:

```bash
kubectl port-forward -n wasmcloud svc/nats 4223:4222
```

## Step 6: Start the data NATS and create the keyvalue buckets

The apps' keyvalue store is a separate NATS, so round 4 can take it down while
the operator keeps talking to the host.

```bash
docker run -d --restart=always --name sreday-nats-data -p 4224:4222 nats:2.12.15-alpine -js

export NATS_DATA=nats://localhost:4224
nats --server $NATS_DATA kv add app-a --storage=file --history=1
nats --server $NATS_DATA kv add app-b --storage=file --history=1
nats --server $NATS_DATA kv add app-c --storage=file --history=1
nats --server $NATS_DATA kv ls
```

## Step 7: Start the OCI registry and push the components

```bash
docker run -d --restart=always -p 5001:5000 --name sreday-registry registry:2.8.3
curl http://localhost:5001/v2/        # expect: {}

cd components/target/wasm32-wasip2/release

oras push localhost:5001/sreday/router:0.1.1 \
  router.wasm:application/vnd.module.wasm.content.layer.v1+wasm \
  --plain-http

oras push localhost:5001/sreday/app-a:0.1.1 \
  app_a.wasm:application/vnd.module.wasm.content.layer.v1+wasm \
  --plain-http

oras push localhost:5001/sreday/app-b:0.1.1 \
  app_b.wasm:application/vnd.module.wasm.content.layer.v1+wasm \
  --plain-http

oras push localhost:5001/sreday/app-c:0.1.1 \
  app_c.wasm:application/vnd.module.wasm.content.layer.v1+wasm \
  --plain-http

cd ../../../..
```

The tags must match `platform/k8s/workload.yaml` (`0.1.1`). Cargo turns hyphens into
underscores, so the file is `app_a.wasm` while the image is `app-a`.

## Step 8: Start the wasmCloud host

Keep this running in its own terminal:

```bash
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 \
OTEL_SERVICE_NAME=wasmcloud-host \
OTEL_RESOURCE_ATTRIBUTES=deployment.environment=sreday-demo \
OTEL_METRIC_EXPORT_INTERVAL=10000 \
RUST_LOG=info \
wash host \
  --host-group sreday \
  --scheduler-nats-url nats://localhost:4223 \
  --data-nats-url nats://localhost:4224 \
  --http-addr 0.0.0.0:8000 \
  --allow-insecure-registries \
  --wasi-otel \
  --max-guest-memory 1536MiB \
  --meters fuel 2>&1 | tee wash.log
```

| Setting | Why |
|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Turns on host telemetry (traces, metrics, logs). There is no `WASMCLOUD_OBSERVABILITY_ENABLED` in v2 |
| `OTEL_METRIC_EXPORT_INTERVAL=10000` | Metrics every 10 s instead of 60 s, for a live dashboard |
| `--wasi-otel` | Lets components send their own spans and logs (`wasi:otel`) |
| `--max-guest-memory 1536MiB` | Memory budget shared by all components (round 3) |
| `--meters fuel` | Measures work done (wasmtime fuel): the dashboard's CPU signal |
| `--allow-insecure-registries` | The local registry is plain HTTP |

In another terminal, check that the host joined the cluster:

```bash
kubectl get hosts -A         # HOSTGROUP sreday, READY True
```

## Step 9: Deploy the workload

```bash
kubectl apply -f platform/k8s/workload.yaml
kubectl get workloaddeployments,workloads    # READY True
```

`platform/k8s/workload.yaml` lists the four components (images, config, allowed
egress hosts) and the host interfaces they use (http, keyvalue, config, otel).
The apps reach toxiproxy at `172.17.0.1` (the Docker bridge): in 2.12.2
`host.wasmcloud.internal` works only for raw sockets, not for outgoing HTTP.

## Step 10: Try it

```bash
curl localhost:8000/healthz        # ok

curl -X POST localhost:8000/t/app-a/invoice-pdf -d '{"amount":120}'
curl -X POST localhost:8000/t/app-b/price-calc  -d '{"sku":"sku-2","qty":1}'
curl -X POST localhost:8000/t/app-c/data-sync   -d '{"batch":1}'
```

### Start steady traffic (k6)

Keep this running in its own terminal; Ctrl+C stops the traffic:

```bash
k6 run --address "" load/tenants.js     # app-a 5/s, app-b 8/s, app-c 3/s, for up to 2 hours
```

`--address ""` turns off k6's own control API (port 6565), which isn't needed.

| What | Where |
|---|---|
| Grafana dashboard | http://localhost:3000 (no login) |
| Router | `POST http://localhost:8000/t/{app-a\|app-b\|app-c}/{function}` |

### Roll out a new version

```bash
(cd components/tenants/app-b && wash build --skip-fetch)
oras push localhost:5001/sreday/app-b:0.1.2 \
  components/target/wasm32-wasip2/release/app_b.wasm:application/vnd.module.wasm.content.layer.v1+wasm \
  --plain-http
# change app-b's image tag to 0.1.2 in platform/k8s/workload.yaml, then:
kubectl apply -f platform/k8s/workload.yaml
kubectl get workloadreplicasets    # a new replica set replaces the old one
```

## Break it, then trace it

The dashboard follows the five SRE signals per tenant (**Latency · Errors · Traffic ·
Saturation · Availability** against a 99.9% SLO), then **Why** (top error messages) and
**Trace / Explain** (failing requests and trace-linked logs).

Run one round at a time with the k6 traffic running. Watch the dashboard, then fix.

| # | Break | Fix | Signal that reacts | What to show |
|---|---|---|---|---|
| 1 | `./chaos/break slow app-a 2000` | `./chaos/fix app-a` | **Latency** | Only app-a p95/p99 jump to ~2 s → click an exemplar ◆ → the long span is `vendor.call` |
| 2 | `./chaos/break crash app-b` | `./chaos/fix app-b` | **Errors, Availability** | Only app-b errors; "Why" shows `index out of bounds`; error budget burns; the failing trace has only a `guest.panic` span |
| 3 | `./chaos/break memory app-c` | `./chaos/fix app-c` | **Saturation** | Memory near the budget, refusals, fuel/s spikes; app-c 507s; everyone's latency rises |
| 4 | `./chaos/break kv` | `./chaos/fix kv` | **Errors, Availability** (all tenants) | Every tenant fails at the same `wasi.keyvalue.*` span: platform, not app |

### Same thing without the scripts

```bash
export NATS_DATA=nats://localhost:4224

# 1  slow vendor
curl -X POST localhost:8474/proxies/app-a-vendor/toxics -H 'content-type: application/json' \
  -d '{"name":"latency","type":"latency","stream":"downstream","attributes":{"latency":2000}}'
curl -X DELETE localhost:8474/proxies/app-a-vendor/toxics/latency

# 2  crash (feature flag in app-b's keyvalue bucket)
nats --server $NATS_DATA kv put app-b flags pricing-v2
nats --server $NATS_DATA kv del app-b flags --force

# 3  memory (flag + a parallel burst)
nats --server $NATS_DATA kv put app-c flags full-resync
for i in $(seq 1 6); do curl -s -o /dev/null -w "%{http_code}\n" -X POST localhost:8000/t/app-c/data-sync -d '{}' & done; wait
nats --server $NATS_DATA kv del app-c flags --force

# 4  keyvalue down (the data NATS; the scheduler NATS keeps running)
docker stop sreday-nats-data
docker start sreday-nats-data
```

## Reset / stop

```bash
./chaos/fix all                                          # undo every break

# stop everything
#   Ctrl+C k6 (Step 10), the wash host (Step 8) and the port-forward (Step 5), then:
kubectl delete -f platform/k8s/workload.yaml
kind delete cluster --name sreday
docker rm -f sreday-registry sreday-nats-data
docker compose down -v
```

A clean slate before going on stage is the stop block above, then Steps 2–10 again.
Removing the registry container also removes the pushed images, so push again (Step 7).
