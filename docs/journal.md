# Journal events

The controller logs every decision to the journal as one line of
`key=value` pairs, each line naming its `event`:

```text
rampire: vm=web alias=ua-virtiomem0 event=controller_decision action=grant session_id=s-1 sequence=4 operation=Add bytes=536870912 grant_bytes=536870912 ...
```

The event names and keys below are stable: a release that renames or
removes one says so in the [changelog](../CHANGELOG.md), and a test fails
if the controller logs an event this page doesn't list. New keys may be
added to an event; read the keys you need and ignore the rest. Values
contain no spaces, except `reason` and `command_result`, which come at the
end of the line.

```bash
journalctl -u rampire -o cat | grep 'event=controller_decision'
```

## Every line about one VM

| Key | Meaning |
|---|---|
| `vm` | The VM |
| `alias` | Its virtio-mem device |

A request is described by `session_id`, `sequence`, `operation` (`Add` or
`Remove`) and `bytes`.

## Requests and decisions

| Event | Keys | When |
|---|---|---|
| `guest_request_observed` | the request, `issued_unix_millis` | A guest's new request is seen |
| `grant_limits` | the request, `available_bytes`, `committed_bytes`, `reserve_bytes`, `margin_bytes`, `guest_maximum_bytes`, `device_capacity_bytes`, `pool_bytes`, `host_room_bytes`, `limiting_factors`, ...; for a removal `minimum_device_bytes`, `removable_bytes`, `limiting_factors` | What bounds a grant |
| `controller_decision` | `action` (`grant` or `refuse`), the request; `grant_bytes`, `pool_ceiling_bytes`, `requested_bytes`, `current_bytes` for a grant; `reason` for a refusal | The decision on a request |
| `resize_requested` | `operation_id`, `direction`, `target_bytes`, `requested_bytes`, `current_bytes`, `command_started_unix_millis`, `command_elapsed_ms`, ... | The device is asked to resize |
| `control_health` | `direction`, the request, `requested_bytes`, `current_bytes`, `remaining_to_device_target_bytes`, `pending_age_ms`, `idle_ms`, `idle_timeout_remaining_ms`, `floor_bytes` | A resize's progress |
| `resize_settled_short` | `operation_id`, `direction`, `target_bytes`, `kept_bytes`, `command_result` | A removal stopped moving and was settled at what the guest kept |
| `held_shrink_released` | `reason` (`guest_low_memory` or `removal_stalled`) | A held removal is let go |
| `request_completed` | `operation_id`, the request, `outcome`, `actual_bytes`, sometimes `reason` | A request's result |
| `ownership_conflict` | `requested_bytes`, `current_bytes`, `reason` | The device changed in a way the controller didn't do |

## Faults

| Event | Keys | When |
|---|---|---|
| `actuation_latched` | `reason` | Resizing stops until `rampire recover` |
| `vm_stopped` | `reason` | The controller stops acting for this VM; others continue |
| `live_state_unavailable` | `reason` | The device's state couldn't be read |
| `guest_input_invalid` | `reason` | No valid publication from the guest |
| `guest_status_unpublished` | `reason` | The host status couldn't reach the guest |
| `publication_copy_failed` | `reason` | The copy for `status`, `top` and `watch` couldn't be written |
| `optional_telemetry_failed` | `reason` | A VM's `telemetry_path` couldn't be written |
| `diagnostic_clock_failed` | `reason` | The clock for a diagnostic couldn't be read |

## The controller and its links

| Event | Keys | When |
|---|---|---|
| `libvirt_connected` | | The libvirt connection is up |
| `control_channel_connected`, `control_channel_lost` | | A VM's control channel comes and goes |
| `control_channel_unavailable` | `reason` | A VM's control channel can't be used |
| `metrics_unwritten` | `reason` | The metrics file couldn't be written; logged once until it can |
| `no_vms` | | The configuration manages no VMs; the controller waits |
