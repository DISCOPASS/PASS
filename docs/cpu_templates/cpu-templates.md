# CPU templates

Firecracker allows users to customise how the vCPUs are represented
to the guest software by changing the following configuration:

- CPUID (x86_64 only)
- MSRs (Model Specific Registers, x86_64 only)
- ARM registers (aarch64 only)

A combination of the changes to the above entities is called a CPU template.

The functionality can be used when a user wants to mask a feature from
the guest. A real world use case for this is representing a heterogeneous
fleet (a fleet consisting of multiple CPU models) as a homogeneous fleet,
so the guests will experience a consistent feature set supported by the host.

> **Note**
Representing one CPU vendor as another CPU vendor is not supported.

> **Note**
CPU templates shall not be used as a security protection against malicious
guests. Disabling a feature in a CPU template does not generally make it
completely unavailable to the guest. For example, disabling a feature related
to an instruction set will indicate to the guest that the feature
is not supported, but the guest may still be able to execute corresponding
instructions if it does not obey the feature bit.

Firecracker supports two types of CPU templates:

- Static CPU templates - a set of built-in CPU templates for users
  to choose from
- Custom CPU templates - users can create their own CPU templates in json
  format and pass them to Firecracker

> **Note**
CPU templates for ARM (both static and custom) require the following patch
to be available in the host kernel: [Support writable CPU ID registers from userspace](https://lore.kernel.org/kvm/20230212215830.2975485-1-jingzhangos@google.com/#t).
Otherwise KVM will fail to write to the ARM registers.

## Static CPU templates

At the moment the following set of static CPU templates are supported:

| CPU template | CPU vendor | CPU model             |
|--------------|------------|-----------------------|
| C3           | Intel      | any                   |
| T2           | Intel      | any                   |
| T2A          | AMD        | Milan                 |
| T2CL         | Intel      | Cascade Lake or newer |
| T2S          | Intel      | any                   |
| V1N1         | ARM        | Neoverse V1           |

T2 and C3 templates are mapped as close as possible to AWS T2 and C3 instances
in terms of CPU features.

The T2S template is designed to allow migrating [snapshots](../snapshotting/versioning.md)
between hosts with Intel Skylake and Intel Cascade Lake securely by further
restricting CPU features for the guest, however this comes with a performance
penalty. Users are encouraged to carry out a performance assessment if they wish
to use the T2S template.

The T2CL template is mapped to be close to Intel Cascade Lake.
It is not safe to use it on Intel CPUs older than Cascade Lake (such as Skylake).

The only AMD template is T2A. It is considered safe to be used with AMD Milan.

Intel T2CL and AMD T2A templates together aim to provide instruction set feature
parity between CPUs running them, so they can form a heterogeneous fleet
exposing the same instruction sets to the application.

The V1N1 template is designed to represent ARM Neoverse V1 as ARM Neoverse N1.

### Configuring static CPU templates

Configuration of a static CPU template is performed via the `/machine-config`
API endpoint:

```bash
curl --unix-socket /tmp/firecracker.socket -i  \
  -X PUT 'http://localhost/machine-config' \
  -H 'Accept: application/json'            \
  -H 'Content-Type: application/json'      \
  -d '{
           "vcpu_count": 2,
           "mem_size_mib": 1024,
           "cpu_template": "T2CL"
  }'
```

## Custom CPU templates

Users can create their own CPU templates by creating a json file containing
modifiers for CPUID, MSRs or ARM registers.

> **Note**
Creating custom CPU templates requires expert knowledge of
CPU architectures. Custom CPU templates must be tested thoroughly before use
in production. An inappropriate configuration may lead to guest crashes or
making guests vulnerable to security attacks. For example, if a CPU template
signals a hardware vulnerability mitigation to the guest while the mitigation
is in fact not supported by the hardware, the guest may decide to disable
corresponding software mitigations which will make the guest vulnerable.

> **Note**
Having MSRs or ARM registers in the custom CPU template does
not affect access permissions that guests will have to those registers.
The access control is handled by KVM and is not influenced by CPU templates.

> **Note**
When setting guest configuration, KVM may reject setting some bits quietly.
This is user's responsibility to make sure that their custom CPU template
is applied as expected even if Firecracker does not report an error.

In order to assist with creation and usage of CPU templates, there exists
a CPU template helper tool. More details can be found
[here](cpu-template-helper.md).

### Configuring custom CPU templates

Configuration of a custom CPU template is performed via the `/cpu-config`
API endpoint.

An example of configuring a custom CPU template on x86_64:

```bash
curl --unix-socket /tmp/firecracker.socket -i  \
  -X PUT 'http://localhost/cpu-config' \
  -H 'Accept: application/json'            \
  -H 'Content-Type: application/json'      \
  -d '{
        "cpuid_modifiers": [
          {
            "leaf": "0x1",
            "subleaf": "0x0",
            "flags": 0,
            "modifiers": [
              {
                "register": "eax",
                "bitmap": "0bxxxx000000000011xx00011011110010"
              }
            ]
          }
        ],
        "msr_modifiers": [
          {
            "addr": "0x10a",
            "bitmap": "0b0000000000000000000000000000000000000000000000000000000000000000"
          }
        ]
      }'
```

This CPU template will do the following:

- in leaf `0x1`, subleaf `0x0`, register `eax`:
  - will clear bits `0b00001111111111000011100100001101`
  - will set bits `0b00000000000000110000011011110010`
  - will leave bits `0b11110000000000001100000000000000` intact.
- in MSR `0x10`, it will clear all bits.

An example of configuring a custom CPU template on ARM:

```bash
curl --unix-socket /tmp/firecracker.socket -i  \
  -X PUT 'http://localhost/cpu-config' \
  -H 'Accept: application/json'            \
  -H 'Content-Type: application/json'      \
  -d '{
        "reg_modifiers": [
          {
            "addr": "0x603000000013c020",
            "bitmap": "0bxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx0000xxxxxxxxxxxx0000xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
          }
        ]
      }'
```

This CPU templates will do the following with the ARM register `0x603000000013c020`:

- will clear bits `0b00000000000000000000000000000000000000000000000000000000000000000000000000001111000000000000111100000000000000000000000000000000`
- will leave bits `0b11111111111111111111111111111111111111111111111111111111111111111111111111110000111111111111000011111111111111111111111111111111`
  intact.

Information on how the ARM register addresses are constructed can be found
in the [KVM API documentation](https://docs.kernel.org/virt/kvm/api.html#kvm-set-one-reg).

### Custom CPU templates language schema

The full description of the custom CPU templates language can be found
[here](schema.json).

#### Expansion of contracted bitmaps

If a contracted version of a bitmap is given, for example, `0b101` where
a 32-bit bitmap is expected, missing characters are implied to be `x`
(`0bxxxxxxxxxxxxxxxxxxxxxxxxxxxxx101`).

#### CPUID normalization and boot protocol register settings

Some of the configuration set by a custom CPU template may be overwritten
by Firecracker. More details can be found [here](cpuid-normalization.md) and
[here](boot-protocol.md).

#### Information about architecture-specific settings

For detailed information when working with custom CPU templates, please
refer to hardware specifications from CPU vendors, for example:

- [Intel Software Developer Manual](https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html)
- [AMD Architecture Programmer's Manual](https://www.amd.com/en/support/tech-docs?keyword=programmer%27s+manual)
- [ARM Architecture Refernce Manual](https://developer.arm.com/documentation/ddi0487/latest)

## A note about configuration of both static and custom CPU templates

If a user configured both a static CPU template (via `/machine-config`) and
a custom CPU template (via `/cpu-config`) in the same Firecracker process,
only the configuration that was performed the _last_ is applied. This means
that if a static CPU template was configured first and a custom CPU template
was configured later, only the custom CPU template configuration will be
applied when starting a microVM.
