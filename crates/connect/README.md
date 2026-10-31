# radixdlt-connect

The former name of [`radixdlt-connect-webrtc`](../connect-webrtc), which this crate re-exports
whole, so that code written against `radixdlt_connect::…` keeps building.

New code depends on:

| Crate | What it does |
|---|---|
| [`radixdlt-connect-auto`](../connect-auto) | talks to the Radix Wallet over iroh or WebRTC, whichever it can — nothing to choose |
| [`radixdlt-connect-webrtc`](../connect-webrtc) | WebRTC only |
| [`radixdlt-connect-iroh`](../connect-iroh) | iroh only |
