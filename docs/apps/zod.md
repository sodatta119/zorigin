# Zod — LAN recon

> **One-liner:** see and inspect everything on your Wi-Fi. "Kneel before Zod" —
> it surveils the whole local network.
>
> Zod is a **diagnostics** app — a different family from Zap/Zulu. It reuses Zap's
> **visual language and build/dist patterns**, but little of Zap's transport core
> (see `docs/apps/README.md`). It has its own scanning engine.

## What it is

An nmap-lite, mobile-friendly local-network scanner + monitor: discover devices,
identify them, probe them, and watch link quality over time. For power users who
want to know what's on their network without a laptop.

## OS reality (read first — it decides platform scope)

- **Android / desktop**: strong. Read the ARP table (`/proc/net/arp`), do an ARP/
  ping sweep of the subnet, TCP-connect port scans, resolve hostnames, map MAC →
  vendor (OUI DB). Android may need Location permission for some Wi-Fi details.
- **iOS**: heavily restricted — **no raw sockets, no ARP**, and the **Local Network
  permission** gates even basic access. You're limited to Bonjour/mDNS browsing +
  TCP connects to known hosts. A full scanner is effectively **Android + desktop
  only**; document iOS as "limited / later".
- **Competition**: Fing dominates here. Zod's edge = Rust speed, clean dark UI,
  privacy (no account, no cloud telemetry), and honesty. Don't out-feature Fing;
  out-trust it.

## Features (prioritized)

1. **Device list** — IP, hostname, MAC, **vendor (OUI)**, up/down, first/last seen.
2. **Ping** — latency to any device.
3. **Port scan** — common-ports TCP connect scan per device; show open services.
4. **Speed / latency / packet-loss monitoring** — periodic samples with a time
   graph. *(This absorbs the old "Speed Logger" idea — it's a Zod tab, not its own
   app.)*
5. **Details** — reverse DNS, TTL/OS hint (best-effort), open-port service labels.
6. **Export / share** a scan snapshot (via Zap, naturally).

## Architecture

- **Scanning engine** in Rust: subnet enumeration from the interface's IP/netmask,
  concurrent ARP/ICMP/TCP probes (bounded concurrency), OUI vendor lookup from a
  bundled table. Keep dependencies lean, like Zap.
- **Front-ends**: desktop (egui, like `zap-desktop`) + Android (Kotlin + JNI, like
  the Zap app). No browser-serving needed — this is a local-only inspector.
- Raw sockets / ICMP need care per-platform (privileges); prefer unprivileged
  approaches (connect-scan, UDP, ARP-table read) where possible.

## Design

Reuse the family look — dark/light, cards, amber (or a distinct "recon" accent).
Reuse the `ZAP_SHOT`-style screenshot harness for GUI verification.

## Non-goals

No cloud inventory, no accounts, no continuous background scanning that drains
battery, no attempt at a full iOS scanner (be upfront about the platform limits).

## First milestone

Desktop + Android: enumerate the subnet, list devices with IP/MAC/vendor/hostname,
and ping. Add port scan and the monitoring graph next.
