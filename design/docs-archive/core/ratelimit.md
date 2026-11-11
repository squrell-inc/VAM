> Moved from the docs site on 2026-10-30: design intent, not built (the Firecracker rate limiter is always `None`).

---
title: Virtio Rate-Limiter
description: Managing resource consumption limits on storage and network interfaces.
---

:::caution[Alpha Stability Warning]
VAM host architecture specifications are currently in **Alpha**. Rate-limiting definitions represent planned configuration structures.
:::

To prevent a single malicious or runaway guest agent from monopolizing disk I/O or network throughput on a shared host node, `vam-core` configures **VirtIO Rate Limiters** at the VMM boundary.

---

## The Token Bucket Algorithm

Firecracker utilizes a standard **Token Bucket** algorithm to throttle MMIO device traffic. 

Every rate-limited device configuration consists of two bucket types:
1. **Bandwidth Bucket**: Limits the raw bytes-per-second throughput (Bytes/s).
2. **Operations Bucket**: Limits the actions-per-second (e.g. IOPS or Network Packets/s).

```text
               Token Flow (Refills at defined rate)
                       │
                       ▼
             ┌──────────────────┐
             │   Token Bucket   │ ◄─── Accumulates up to Max Size
             └────────┬─────────┘
                      │
   Block I/O Request  │ (Consumes tokens equivalent to bytes/ops)
  ───────────────────►┼───► Allowed through to hardware
                      │
             [Bucket Exhausted?]
                      │
                      ▼
               Request Queued /
           Execution Backpressured
```

---

## Structuring the Token Bucket

Each bucket configuration is structured inside `config.rs` and maps to Firecracker’s device JSON specifications:

```json
{
  "size": 10485760,
  "one_time_burst": 52428800,
  "refill_time": 1000
}
```

### Parameter Reference

* **`size`**: The target capacity of the bucket (in bytes or operation units). This acts as the sustained limit.
* **`one_time_burst`**: The maximum volume allowed in a single sudden burst before throttling triggers.
* **`refill_time`**: The token generation frequency in milliseconds (typically set to `1000ms` for standard per-second limits).

---

## Applying Limits to Devices

Rate limiters are configured on a per-device basis during the **Booting** phase:

### 1. Throttling Block Devices (`PUT /drives/{drive_id}`)
Applies bounds to guest disk I/O, protecting Drive9 volume host performance.

```json
{
  "drive_id": "rootfs",
  "path_on_host": "rootfs.ext4",
  "is_root_device": true,
  "rate_limiter": {
    "bandwidth": {
      "size": 10485760,
      "one_time_burst": 52428800,
      "refill_time": 1000
    },
    "ops": {
      "size": 500,
      "one_time_burst": 1000,
      "refill_time": 1000
    }
  }
}
```
*The above configuration caps sustained disk throughput to **10 MB/s** with an allowed one-time burst of **50 MB**, and limits sustained read/write operations to **500 IOPS**.*

### 2. Throttling Network Interfaces (`PUT /network-interfaces/{id}`)
Limits network packet and bandwidth consumption to prevent network flooding or denial-of-service attempts.

```json
{
  "iface_id": "net0",
  "host_dev_name": "tap0",
  "rate_limiter": {
    "bandwidth": {
      "size": 5242880,
      "one_time_burst": 20971520,
      "refill_time": 1000
    }
  }
}
```
*This configuration limits guest networking output to a sustained **5 MB/s** (40 Mbps) speed.*
