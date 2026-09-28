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

**App**:
A named, long-lived workload that owns a route (hostname → port) and has at most one VM at a time.
_Avoid_: service, deployment

**Park / wake**:
Snapshot an App's VM to disk and free its RAM / start a new VM for the App from that snapshot.
_Avoid_: suspend, hibernate, resume

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
