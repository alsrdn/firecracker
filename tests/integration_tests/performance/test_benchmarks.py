import logging
import pytest

from conftest import _test_images_s3_bucket
from framework.artifacts import ArtifactCollection, ArtifactSet
from framework.matrix import TestMatrix, TestContext
from framework.utils_cpuid import get_cpu_model_name, get_instance_type
from framework.builder import MicrovmBuilder
from framework.utils import get_cpu_percent, get_kernel_version,\
    run_cmd, CpuMap, CmdBuilder, DictQuery
import host_tools.network as net_tools

TEST_ID = "unixbench_test"
MAKE = "make"

@pytest.mark.timeout(500)
def test_unixbench_syscalls(bin_cloner_path, results_file_dumper):
    """
    Test results of running Unixbench syscalls.

    @type: performance
    """
    logger = logging.getLogger(TEST_ID)
    artifacts = ArtifactCollection(_test_images_s3_bucket())
    microvm_artifacts = ArtifactSet(artifacts.microvms(keyword="2vcpu_1024mb"))
    #microvm_artifacts.insert(artifacts.microvms(keyword="2vcpu_1024mb"))
    kernel_artifacts = ArtifactSet(artifacts.kernels(keyword="4.14"))
    disk_artifacts = ArtifactSet(artifacts.disks(keyword="bionic"))

    print(str(len(disk_artifacts)))

    print("Testing on processor %s", get_cpu_model_name())

    # Create a test context and add builder, logger, network.
    test_context = TestContext()
    test_context.custom = {
        'builder': MicrovmBuilder(bin_cloner_path),
        'logger': logger,
        'name': TEST_ID,
        'results_file_dumper': results_file_dumper
    }

    test_matrix = TestMatrix(context=test_context,
                             artifact_sets=[
                                 microvm_artifacts,
                                 kernel_artifacts,
                                 disk_artifacts
                             ])
    test_matrix.run_test(unixbench_workload)


def get_next_core(used_cores, current_core=None):
    if current_core is None:
        current_core = 0
    else:
        current_core += 1

    while current_core in used_cores:
        current_core += 1

    with open(f"/sys/devices/system/cpu/cpu{current_core}/topology/thread_siblings_list") as f:
        siblings_str = f.readline()
        siblings = {int(x) for x in siblings_str.split(",")}
        for s in siblings:
            used_cores.add(s)

    print(used_cores)

    return current_core


def unixbench_workload(context):
    """Iperf between guest and host in both directions for TCP workload."""
    vm_builder = context.custom['builder']
    logger = context.custom["logger"]
    file_dumper = context.custom['results_file_dumper']

    # Create a rw copy artifact.
    rw_disk = context.disk.copy()
    # Get ssh key from read-only artifact.
    ssh_key = context.disk.ssh_key()
    # Create a fresh microvm from artifacts.
    vm_instance = vm_builder.build(kernel=context.kernel,
                                   disks=[rw_disk],
                                   ssh_key=ssh_key,
                                   config=context.microvm,
                                   cpu_template="T2")
    basevm = vm_instance.vm
    basevm.start()
    custom = {
        "microvm": context.microvm.name(),
        "kernel": context.kernel.name(),
        "disk": context.disk.name(),
        "cpu_model_name": get_cpu_model_name()
    }

    # Check if the needed CPU cores are available. We have the API thread, VMM
    # thread and then one thread for each configured vCPU.
    assert CpuMap.len() >= 2 + basevm.vcpus_count

    # Pin uVM threads to physical cores.
    used_cpus = set()
    core = get_next_core(used_cpus)
    print(f"Pinning on core {core}")
    assert basevm.pin_vmm(core), \
        "Failed to pin firecracker thread."

    core = get_next_core(used_cpus, core)
    print(f"Pinning on core {core}")
    assert basevm.pin_api(core), \
        "Failed to pin fc_api thread."
    for i in range(basevm.vcpus_count):
        core = get_next_core(used_cpus, core)
        print(f"Pinning on core {core}")
        assert basevm.pin_vcpu(i, core), \
            f"Failed to pin fc_vcpu {i} thread."

    print("Testing with microvm: \"{}\", kernel {}, disk {}"
                .format(context.microvm.name(),
                        context.kernel.name(),
                        context.disk.name()))

    ssh_connection = net_tools.SSHConnection(basevm.ssh_config)
    make_cmd = f"cd ~/byte-unixbench-master/UnixBench; make"
    errcode, _, _ = ssh_connection.execute_command(make_cmd)
    assert errcode == 0

    _, stdout, _ = ssh_connection.execute_command(f"cat /sys/devices/system/cpu/vulnerabilities/spectre_v2")
    print(str(stdout.read()))

    unixbench_cmd = f"cd ~/byte-unixbench-master/UnixBench; ./Run syscall"
    errcode, stdout, stderr = ssh_connection.execute_command(unixbench_cmd)
    print(str(stdout.read()))
    print("\n------\n")
    print(str(stderr.read()))