---
title: Windows + WSL (Local Linux Node)
description: Run the full VAM stack on Windows using WSL2 (Ubuntu). This guide mirrors the macOS + Lima page but uses PowerShell and WSL2.
---

## Requirements

| Requirement | Notes |
|---|---|
| **OS** | Windows 10 (2004+) or Windows 11 |
| **WSL2** | Enabled with Ubuntu 24.04 (or later) distribution |
| **Hardware** | CPU virtualization support (VT‑x/AMD‑V) |
| **RAM** | At least 8 GiB allocated to the WSL VM |
| **Disk** | ~40 GiB for the Linux file system |
| **PowerShell** | For running setup commands |

## 1. Install WSL & Ubuntu

```powershell
# Enable WSL and set version 2
wsl --install -d Ubuntu
wsl --set-default-version 2

# Verify installation
wsl -l -v
```

## 2. Prepare the Linux environment (inside WSL)

```bash
# Launch Ubuntu
wsl -d Ubuntu

# Inside the Linux shell
sudo apt update && sudo apt install -y build-essential curl git libssl-dev pkg-config

# Install Rust (stable)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source $HOME/.cargo/env
```

## 3. Build the VAM CLI

```bash
# From the repository root (mounted under /mnt/c/Users/<you>/vam)
cd /mnt/c/Users/<you>/vam
cargo build -p vam-cli --release
# Optionally copy to Windows PATH
cp target/release/vam /mnt/c/Users/<you>/.local/bin/
```

## 4. Prepare guest kernel & rootfs

```bash
# In the same repo directory
./scripts/fetch-guest-kernel.sh   # fetches x86_64 vmlinux
./scripts/build-rootfs.sh         # builds rootfs.ext4 (≈10‑20 min first run)
```

## 5. Start `vamd` inside WSL

```bash
./scripts/start-vamd-wsl.sh   # launches the daemon, builds aarch64 (if needed) and starts Firecracker
```

The daemon will listen on `0.0.0.0:8080` inside WSL, which is automatically forwarded to `127.0.0.1:8080` on the Windows host.

## 6. Configure the Windows CLI

```powershell
# Set env var for the bootstrap key (or use your own token)
$env:VAMD_BOOTSTRAP_API_KEY = "vam_sk_dev_18cd74a2a97ff4d1"

# Point CLI at the forwarded port
vam config set-host https://127.0.0.1:8080   # vamd serves TLS by default; trust a self-signed node with: vam config set-ca <state dir>/tls/ca.pem
vam config set-key --from-env VAMD_BOOTSTRAP_API_KEY

# Verify health
vam health
```

## 7. Run a sample session

```powershell
vam run --file examples/bun-assistant/run.json --prompt "List workspace files" --follow
```

## 8. Troubleshooting

- **Port conflicts:** Ensure no other service is listening on `8080`.
- **/dev/kvm permissions:** WSL2 provides a virtual KVM; no extra setup needed.
- **Firewall:** Allow inbound traffic on port 8080 for the `vamd.exe` process if Windows Defender blocks it.

## 9. Clean up

```bash
# Inside WSL
pkill vamd
exit
# From PowerShell, optionally shut down WSL
wsl --shutdown
```

---
For a remote‑only workflow (no WSL), see the [Installation & Setup](/getting-started/installation/) page.
