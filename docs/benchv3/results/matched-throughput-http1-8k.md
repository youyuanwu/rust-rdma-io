# Open-loop matched throughput — HTTP/1.1 · 8 KiB

Every HTTP/1.1 transport at one shared 50k req/s offered load (1× vCPU / 64 conn).

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** HTTP/1.1 · **payload:** 8 KiB · **connections:** 1× vCPU · **load:** open-loop matched-throughput · **duration/warmup:** 10 s / 3 s · **git commit:** `2bbfd3d` · **date:** `20260728T214624Z`

| transport | target rps | achieved rps | p50 (µs) | p99 (µs) | p99.9 (µs) | CPU/op (µs) | cores | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `send-recv` | 50000 | 49994 | 1036.0 | 2275.0 | 2451.0 | 32.96 | 1.65 | 0 |
| `read-ring` (arm-park) | 50000 | 49999 | 1033.0 | 2267.0 | 2483.0 | 33.08 | 1.65 | 0 |
| `read-ring` (busy-poll) | 50000 | 50000 | 564.0 | 1056.0 | 1067.0 | 1279.47 | 63.97 | 0 |
| `read-ring` (thread-per-core park) | 50000 | 50000 | 962.0 | 2042.0 | 2143.0 | 32.08 | 1.60 | 0 |
| `credit-ring` | 50000 | 49996 | 1011.0 | 2215.0 | 2417.0 | 35.08 | 1.75 | 0 |
| kernel baseline | 50000 | 50000 | 1045.0 | 2275.0 | 2487.0 | 37.36 | 1.87 | 0 |
