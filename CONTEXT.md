# Cirrocumulus

A self-hosted mini cloud: every workload runs in its own jailed Firecracker microVM,
managed from the `cirro` terminal CLI.

## Hosts and processes

**Node**:
A KVM-capable Linux host that runs a Node agent. All Nodes are identical.
_Avoid_: host (when meaning a Cirrocumulus member), server, worker

**Node agent**:
The long-running, privileged `cirro node` process on a Node that owns every VM on it and the host state they need.
_Avoid_: daemon, VM manager, node service

**Server**:
The control-plane role (`cirro server`) that owns desired state across Nodes.
_Avoid_: controller, master

## Workloads

**VM**:
One running Firecracker microVM with a user-chosen name, owned by exactly one Node agent. A VM ends when its microVM stops; it is never restarted as "the same VM".
_Avoid_: instance, machine, container, guest (except for what runs inside)

**Ended VM**:
The record a VM leaves behind once it has stopped and its host state is gone: name, end time, end reason and console log. It is history, not a VM, and is replaced when its name is reused.
_Avoid_: stopped VM, dead VM, exited container

**Image**:
An OCI image in a registry, named by a reference such as `nginx:alpine`, that a VM can be started from.
_Avoid_: container image, Docker image (except when quoting Docker)

**Rootfs**:
The bootable ext4 filesystem a VM runs on, with guest-init as `/init`: built from an Image, or supplied by hand.
_Avoid_: disk image, root disk

**App**:
A named, long-lived workload that owns a Route and has at most one VM at a time. On a single Node it is a VM started with a Route; the Route stays with the name through park, wake and the VM ending, until `rm` or a new `run` under the name, which brings its own Route or none.
_Avoid_: service, deployment

**Route**:
A hostname and the port inside the App's VM that requests for it go to. A hostname belongs to at most one App on a Node.
_Avoid_: ingress, virtual host, mapping

**Park / wake**:
Snapshot a VM to disk and end it, freeing its RAM / start a new VM under the same name from that snapshot. Parking an App parks its VM.
_Avoid_: suspend, hibernate, resume

**Parked VM**:
An Ended VM whose end was a park, so it still holds a snapshot of the guest's memory, devices and Rootfs. Waking it uses up the snapshot; `rm` discards it. Its name can't be taken by a new `run` while the snapshot exists.
_Avoid_: paused VM, sleeping VM, stopped VM

## Networking

**Node subnet**:
The /24 of VM addresses a Node hands out, carved from the Cirrocumulus address range (`10.77.0.0/16` by default).
_Avoid_: pool, VM network

**VM address**:
The unique address from the Node subnet by which the Node reaches one VM. A woken App's new VM may get a different VM address.
_Avoid_: VM IP, host address

**Guest address**:
The fixed address the guest operating system configures on its own interface, identical inside every VM.
_Avoid_: internal IP, VM IP

**Edge**:
The HTTP(S) reverse proxy in the Node agent that receives requests from outside the Node and sends each to the App whose Route names its hostname.
_Avoid_: router (except the `Router` trait the edge asks), load balancer, ingress controller
