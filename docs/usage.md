# Usage guide

This guide explains how to use virtio-mem-pool in plain language. It
covers what the program does, where its settings are, how to change them
and how to check that everything is working. You do not need to know how
the program works inside.

Every command in this guide is typed into a terminal on the **host**, the
physical computer that runs your virtual machines. Copy each one exactly,
changing only the parts this guide tells you to change.

## What the program does

Your host has a fixed amount of memory (RAM) to share between its virtual
machines (VMs). Normally each VM is given a set amount and keeps all of
it, even when it is sitting idle.

This program shares the memory out as it is needed instead:

- Each VM starts small.
- When a VM gets busy and runs low on memory, it asks for more, and the
  program gives it more.
- When the VM quietens down, it hands the spare memory back, so another
  VM can use it.

The memory the VMs share is called the **pool**. You decide how big the
pool is, how much each VM may have, and which VMs come first when memory
is short.

## Words used in this guide

| Word | Meaning |
|---|---|
| **Host** | The physical computer that runs the VMs |
| **VM** | A virtual machine running on the host |
| **Pool** | The memory the VMs share |
| **Controller** | The part of the program on the host that hands out memory. It runs all the time in the background |
| **Guest service** | The small part of the program inside each VM. It works out when the VM needs more or less memory and asks the controller |
| **Priority** | A number you give each VM. When there isn't enough memory for everyone, the VM with the higher number is served first |
| **Instance name** | The name the controller was installed under. You need it for some commands ([how to find it](#find-your-instance-name)) |

## Before you start

You need:

- A terminal on the host, logged in as a user who can use `sudo`
  (`sudo` runs a command as an administrator, and asks for your password).
- The project folder, the copy of this project on the host that the
  controller was installed from. This guide calls it `~/virtio-mem-pool`.
  If yours is somewhere else, use that path instead.

### Find your instance name

Type:

```bash
systemctl list-units --all 'virtio-mem-host@*'
```

You will see a line like this:

```text
virtio-mem-host@main.service   loaded active running   Virtio-mem controller for main
```

The instance name is the part between `@` and `.service`. Here it is
`main`. This guide uses `main` in its examples. Use your own name instead.

### Find the names of your VMs

Type:

```bash
sudo virsh list --all
```

The `Name` column lists every VM on the host. The program uses exactly
these names.

## Where the settings are

The settings are plain text files in this folder on the host:

```text
/etc/virtio-mem-host/
```

| File | What it controls |
|---|---|
| `global-settings.toml` | Settings for the whole host: the pool size and how much memory the host keeps for itself |
| `host_list.toml` | Which VMs the program manages, and each VM's **priority** |
| `<vm name>_settings.toml` | One file per VM: the smallest and largest that VM may be, and how much spare memory it keeps. For a VM called `webserver`, the file is `webserver_settings.toml` |
| `attestations/` | A folder the program manages. It holds a record for each VM showing it was checked as safe to resize. **Don't change anything in it** |

Each settings file has notes in it explaining its settings. To see what's
there, type:

```bash
ls /etc/virtio-mem-host/
```

To read a file without changing it, type `sudo cat` and the file's name:

```bash
sudo cat /etc/virtio-mem-host/host_list.toml
```

## How to edit a settings file

Open the file with the `nano` text editor. The files belong to the
administrator, so start the command with `sudo`:

```bash
sudo nano /etc/virtio-mem-host/host_list.toml
```

In `nano`, move around with the arrow keys and type to change text.
Press **Ctrl+O** then **Enter** to save, and **Ctrl+X** to leave.

- Lines that start with `#` are notes for people. The program ignores
  them. You can add notes of your own the same way, and they are kept.
- Each setting is a name, `=`, and a value. Change only the value.
- Write sizes in quotes, with a unit: `"512MB"`, `"128MiB"`, `"3GB"`
  ([more about sizes](#writing-sizes)).

Saving the file does **not** change anything yet. The program keeps
using the old settings until you apply the change
([How to change a setting](#how-to-change-a-setting)). Applying checks
the file first, so a mistake is caught before it can take effect.

## The settings explained

### Pool size and host reserve: `global-settings.toml`

```toml
pool_bytes = "16GB"
host_min_headroom_bytes = "4GB"
observe_only = false
```

| Setting | What it means | How to choose |
|---|---|---|
| `pool_bytes` | The total extra memory all the VMs may share, on top of the small amount each one starts with | The host's memory, minus what the host needs for itself, minus each VM's starting memory |
| `host_min_headroom_bytes` | Memory the host always keeps free for itself. The program never gives VMs memory that would take the host below this | `"4GB"` suits most hosts. Raise it if the host runs other programs |
| `observe_only` | Trial mode. `true`: the program decides and logs everything but changes no VM's memory | `false` for normal use |

The file has a few more settings, marked as technical. Leave them alone
unless the [administration guide](administration.md#global-settingstoml)
tells you otherwise.

### Which VMs, and their priority: `host_list.toml`

There is one block per VM. Each block starts with `[[vm]]`:

```toml
[[vm]]
vm_name = "webserver"
alias = "ua-virtiomem0"
priority = 100

[[vm]]
vm_name = "backup"
alias = "ua-virtiomem0"
priority = 50
```

| Setting | What it means |
|---|---|
| `vm_name` | The VM's name, exactly as `virsh list` shows it. Don't change it |
| `alias` | An internal name for the VM's memory device. Don't change it |
| `priority` | Who comes first when memory is short |

#### Setting a priority

Priority is a whole number above zero. **A higher number means more
important.**

- When there is plenty of memory in the pool, priority makes no
  difference. Every VM gets what it asks for.
- When the pool is running short, a VM with a higher priority gets the
  memory first.
- Two VMs with the same priority are served in the order they asked.
- The program never takes memory away from one VM to give it to another.
  A low-priority VM keeps what it already has, but it may have to wait
  for more.

In the example above, if `webserver` (100) and `backup` (50) both ask for
memory when only a little is left, `webserver` gets it.

Spacing the numbers out (100, 50, 10) leaves room to add a VM between
them later.

### Limits for one VM: `<vm name>_settings.toml`

```toml
vm_name = "webserver"
guest_os = "linux"
min_ram = "2GB"
max_ram = "8GB"
free_ram = "512MB"
```

| Setting | What it means | How to choose |
|---|---|---|
| `vm_name` | The VM's name. Must match the file name and `host_list.toml`. Don't change it | — |
| `guest_os` | The VM's operating system. Don't change it | — |
| `min_ram` | The smallest the VM is ever made. Memory is never taken back below this | Usually the amount the VM starts with |
| `max_ram` | The largest the VM may ever grow | The most this VM should ever have. For a Linux VM, keep it no more than about four times the memory the VM starts with, or the VM may not be able to give memory back reliably |
| `free_ram` | How much spare memory the VM keeps ready. It asks for more when its spare falls below this, and hands memory back when it has more spare than this | `"512MB"` to `"1GB"` suits most VMs. More makes the VM respond faster to sudden demand but keeps more memory idle. Less shares memory more tightly |

All three sizes cover the VM's total memory, including what it starts
with.

### Writing sizes

| You can write | Meaning |
|---|---|
| `"512MB"` or `"512MiB"` | 512 megabytes |
| `"3GB"` or `"3GiB"` | 3 gigabytes |
| `"1.5GB"` | 1.5 gigabytes (1536 MB) |
| `"3 GB"` or `"3gb"` | Also fine: spaces and lower case are allowed |

Here, MB and MiB mean the same thing, and 1 GB is 1024 MB. Use whole
gigabytes, or multiples of 16 MB (`"512MB"`, `"768MB"`, `"1536MB"`). If a
size can't be used, applying the change tells you which one.

Always include the unit. A number on its own, such as `max_ram = 8`,
means 8 **bytes**, and is refused.

## How to change a setting

The steps are always the same: edit the file, apply the change, check it.

**Example: make `backup` more important than `webserver`.**

1. Open the host list:

   ```bash
   sudo nano /etc/virtio-mem-host/host_list.toml
   ```

2. Find the `backup` block and change `priority = 50` to
   `priority = 150`. Save with **Ctrl+O**, **Enter**, then leave with
   **Ctrl+X**.

3. Apply the change. Use your own instance name in place of `main`:

   ```bash
   cd ~/virtio-mem-pool
   deploy/scripts/install-host.sh /etc/virtio-mem-host main
   ```

   It asks for your `sudo` password, and can take a few minutes the
   first time. It then:
   1. briefly stops the controller,
   2. checks all your settings,
   3. installs them, keeping a backup of the old ones,
   4. starts the controller again.

   Your VMs keep running throughout and keep the memory they have.

4. Check that it worked. The script ends by showing the controller's
   state, which should say **`active (running)`** in green. Then look at
   the pool ([Checking that it's working](#checking-that-its-working)).

**Example: let `webserver` grow to 12 GB.** Run
`sudo nano /etc/virtio-mem-host/webserver_settings.toml`, change
`max_ram = "8GB"` to `max_ram = "12GB"`, save, and apply exactly as in
step 3 above.

### If applying reports an error

The message says which file and setting is wrong, for example a misspelt
setting name or a size that can't be used. **The controller is left
stopped** until a change is applied. Your VMs keep running with the
memory they have, but they don't grow or shrink in the meantime.

Open the file again, fix the mistake, save, and apply again (step 3).

## Checking that it's working

### Is the controller running?

```bash
systemctl status virtio-mem-host@main
```

Look for **`active (running)`**. Press **q** to get back to the prompt.

### See the pool live

The pool view shows every VM and how the memory is shared, updating every
second:

```bash
cd ~/virtio-mem-pool
cargo build --release -p virtio-mem-xtask
sudo target/release/virtio-mem-xtask pool --config /etc/virtio-mem-host/host_list.toml
```

(The `cargo build` line is needed only the first time, and after you
update the project folder.)

How to read it:

- **Pool**, at the top: one bar for the whole pool. Each VM's share is in
  its own colour, and the rest of the bar is free memory. Below the bar
  is how much is in use, how much is free, and how much memory the host
  has left.
- **VMs**: one line per VM, showing its priority, its current size,
  whether it is growing or shrinking, how much of its memory is in use,
  and any request it is waiting on. A VM marked **stale** hasn't reported
  in for over 30 seconds.
- **Pool history**: a chart of how the pool's use has changed.
- **Pool events**: the latest decisions, such as memory given, refused or
  returned.

Press **Ctrl+C** to leave.

You can open the same view from a Windows PC. See
[Watching the pool](tools.md#watching-the-pool).

### What has the controller been doing?

```bash
journalctl -u virtio-mem-host@main --since "1 hour ago"
```

This shows the controller's log. Press **Space** to page down, and **q**
to leave. To watch new lines as they happen, use
`journalctl -u virtio-mem-host@main -f` and press **Ctrl+C** to stop.

## Stopping and starting

| To | Type |
|---|---|
| Stop the controller | `sudo systemctl stop virtio-mem-host@main` |
| Start it again | `sudo systemctl start virtio-mem-host@main` |

While the controller is stopped, your VMs keep running with the memory
they have. They don't grow or shrink until it starts again. Nothing is
lost: requests the VMs made while it was stopped are handled when it
starts.

## When something goes wrong

| What you see | What it means | What to do |
|---|---|---|
| `systemctl status` says **failed** | The controller found a problem and stopped itself rather than risk a mistake. It doesn't restart on its own | Look at the log (`journalctl -u virtio-mem-host@main -n 50`). If the cause is clear and fixed, start it again. If not, get help from whoever set it up |
| A VM never grows | The pool is full, the host is at its reserve, or the VM is at its `max_ram` | Look at the pool view. Raise `pool_bytes` or the VM's `max_ram`, or lower other VMs' `max_ram` |
| A VM grows, but slowly gives memory back | Normal. A VM returns memory gradually, and can take several minutes when the memory was busy | Wait |
| A VM stays large after a restart | A restart from inside the VM keeps its size | Shut the VM down fully, then start it |
| The log mentions **latched** | The controller stopped resizing after something it couldn't confirm was safe | It needs a person to check it. See [Recovering](administration.md#recovering) |
| The log mentions **drifted** | The VM's setup was changed after it was checked | The VM must be checked again. See [Adding a VM](administration.md#adding-a-vm), step 4 |
| A VM's line in the pool view says **stale** | The VM's guest service hasn't reported in | Check that the VM is running. Inside the VM, `systemctl status virtio-mem-guest` should say `active (running)` |

**Never delete files under `/var/lib/virtio-mem-host/` or
`/var/lib/virtio-mem-guest/`, or in `/etc/virtio-mem-host/attestations/`.**
The program needs them to keep each VM in the right state.

## Going further

These jobs need more technical knowledge, and the
[administration guide](administration.md) covers them in full:

- adding a new VM, or removing one,
- installing the controller for the first time,
- recovering a controller that has latched,
- upgrading to a new version.

For recording a session in detail, or other monitoring tools, see
[tools](tools.md).
