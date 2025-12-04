API_SOCKET="/tmp/firecracker.socket"

# Re-configure pmem device BEFORE loading snapshot
# Raw_memory pmem devices are not saved in snapshots and must be reconfigured
echo "Configuring pmem device..."
sudo curl --unix-socket "${API_SOCKET}" -i \
    -X PUT 'http://localhost/pmem/pmem0' \
    -H 'Accept: application/json' \
    -H 'Content-Type: application/json' \
    -d "{
         \"id\": \"pmem0\",
         \"path_on_host\": \"/tmp/firecracker-pmem\",
         \"root_device\": false,
         \"read_only\": false,
         \"raw_memory\": true
    }"

echo "Loading snapshot..."
sudo curl --unix-socket "${API_SOCKET}" -i \
    -X PUT 'http://localhost/snapshot/load' \
    -H  'Accept: application/json' \
    -H  'Content-Type: application/json' \
    -d '{
            "snapshot_path": "./snapshot/snapshot_file",
            "mem_backend": {
                "backend_path": "./snapshot/mem_file",
                "backend_type": "File"
            },
            "enable_diff_snapshots": false,
            "resume_vm": false
    }'

echo "Resuming VM..."
sudo curl --unix-socket "${API_SOCKET}" -i \
    -X PATCH 'http://localhost/vm' \
    -H 'Accept: application/json' \
    -H 'Content-Type: application/json' \
    -d '{
            "state": "Resumed"
    }'
