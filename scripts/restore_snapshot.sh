API_SOCKET="/tmp/firecracker.socket"


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


sudo curl --unix-socket "${API_SOCKET}" -i \
    -X PATCH 'http://localhost/vm' \
    -H 'Accept: application/json' \
    -H 'Content-Type: application/json' \
    -d '{
            "state": "Resumed"
    }'