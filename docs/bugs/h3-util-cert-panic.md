# h3-util Panic on TLS Certificate Error

## Summary

When using `tonic-h3` (v0.0.5) with a self-signed certificate that has
`CA:TRUE` in basic constraints, the QUIC handshake fails with a rustls
`CaUsedAsEndEntity` error. However, instead of returning a proper error,
`h3-util` panics with:

```
thread 'tokio-rt-worker' panicked at h3-util-0.0.5/src/client_conn.rs:136:56:
`async fn` resumed after completion
```

## Root Cause

1. Client creates an `H3Channel` and calls `say_hello()`
2. Quinn/rustls rejects the server cert during QUIC handshake
   (`CaUsedAsEndEntity` — cert has `CA:TRUE`, rustls requires `CA:FALSE`
   for end-entity certs)
3. The h3 connection driver task fails but the `SendRequest` future
   has already been polled to completion
4. The next poll of the connection resumes the completed `async fn`,
   triggering a Rust panic

## Symptom

- Zero successful requests
- Panic message: `` `async fn` resumed after completion ``
- Location: `h3-util-0.0.5/src/client_conn.rs:136:56`
- The real error (`CaUsedAsEndEntity`) is hidden by the panic

## Fix

Generate the certificate with `CA:FALSE`:

```bash
openssl req -x509 -newkey rsa:2048 -keyout key.pem -out cert.pem \
    -days 3650 -nodes -subj "/CN=rdma-bench" \
    -addext "basicConstraints=critical,CA:FALSE" \
    -addext "subjectAltName=DNS:localhost,IP:127.0.0.1"
```

Note: OpenSSL's TLS implementation (`tonic-tls` mode) accepts `CA:TRUE`
certs without error. Only rustls (used by Quinn/H3) enforces this.

## Affected Components

- `tonic-h3` v0.0.5
- `h3-util` v0.0.5
- `quinn` with rustls backend
- Any self-signed cert generated with `openssl req -x509` (defaults to `CA:TRUE`)

## Lesson

When using both OpenSSL (tonic-tls) and rustls (quinn/h3) with the same
certificate, generate with `CA:FALSE` to satisfy rustls's stricter
validation. The misleading panic in h3-util makes this hard to diagnose —
always check cert properties first when H3 connections fail mysteriously.
