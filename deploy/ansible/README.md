# Running RAMpire from configuration management

An example Ansible role (roadmap O4.4). Keep each host's configuration in
git, beside the playbook:

```text
example/
  ansible.cfg            # finds ../roles
  playbook.yml
  files/global.toml
  files/vms.d/NAME.toml
  files/reviews/PROFILE.toml
```

```bash
cd example
ansible-playbook -i kvm-host.example, playbook.yml
```

The role installs the package, copies the files into `/etc/rampire`,
runs `rampire check`, then `rampire apply --yes --detailed-exitcode`. It
reports "changed" only when the controller's configuration changed, and
a configuration that fails its checks changes nothing on the host.

Attestations are made on the host: `rampire apply` attests a running VM
that has none, and after a change to a VM's definition a person runs
`rampire vm attest VM`. Set `rampire_manage_attestations: true` only if
you copy them back into git each time. A VM taken out of git is taken out
of RAMpire with `rampire vm remove VM`.

[cloud-init/rampire-guest.yaml](../cloud-init/rampire-guest.yaml)
installs the guest service when a Linux VM is first created.
