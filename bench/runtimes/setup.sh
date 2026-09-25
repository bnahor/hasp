#!/bin/bash
# Inside the Lima VM (rtbench.yaml): Docker, gVisor, Kata 4.2 (Cloud Hypervisor + Firecracker).
set -e
curl -fsSL https://get.docker.com | sudo sh
curl -fsSL https://gvisor.dev/archive.key | sudo gpg --dearmor -o /usr/share/keyrings/gvisor-archive-keyring.gpg
echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/gvisor-archive-keyring.gpg] https://storage.googleapis.com/gvisor/releases release main" \
  | sudo tee /etc/apt/sources.list.d/gvisor.list
sudo apt-get update -qq && sudo apt-get install -y -qq runsc zstd && sudo runsc install
cd /tmp
for t in kata-static kata-go-static; do
  curl -fsSL -o $t.tar.zst https://github.com/kata-containers/kata-containers/releases/download/4.2.0/$t-4.2.0-arm64.tar.zst
  sudo tar --zstd -xf $t.tar.zst -C /
done
sudo ln -sf /opt/kata/bin/containerd-shim-kata-v2 /usr/local/bin/containerd-shim-kata-v2
# Kata 4.2 accepts only shipped config paths in KATA_CONF_FILE; select a
# hypervisor with the ConfigPath runtime option instead. dial_timeout is
# raised because a 4 GB VM boots slowly under nested virtualization.
sudo cp /opt/kata/share/defaults/kata-containers/configuration-clh.toml /etc/kata-clh-bench.toml
sudo sed -i 's/^dial_timeout *=.*/dial_timeout = 300/' /etc/kata-clh-bench.toml
sudo python3 - <<PY
import json
p="/etc/docker/daemon.json"; d=json.load(open(p))
d["runtimes"]["kata-clh"]={"runtimeType":"io.containerd.kata.v2","options":{"ConfigPath":"/etc/kata-clh-bench.toml"}}
json.dump(d,open(p,"w"),indent=2)
PY
sudo systemctl restart docker
docker build -t rtbench:1 "$(dirname "$0")"
