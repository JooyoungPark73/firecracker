UFFD_SOCKET="/tmp/firecracker-uffd.socket"

# Pause vm for snapshotting

sudo curl --unix-socket /tmp/firecracker.socket -i \
-X PATCH 'http://localhost/vm' \
    -H 'Accept: application/json' \
    -H 'Content-Type: application/json' \
    -d '{
            "state": "Paused"
    }'

sleep 1s

# Cleanup snapshot and mem_file
# sudo rm ./snapshot/snapshot_file ./snapshot/mem_file

sleep 1s


# Capture full snapshot
sudo curl --unix-socket /tmp/firecracker.socket -i \
    -X PUT 'http://localhost/snapshot/create' \
    -H  'Accept: application/json' \
    -H  'Content-Type: application/json' \
    -d '{
            "snapshot_type": "Full",
            "snapshot_path": "./snapshot/snapshot_file",
            "mem_file_path": "./snapshot/mem_file"
    }'

# # Capture diff snapshot
# sudo curl --unix-socket /tmp/firecracker.socket -i \
#     -X PUT 'http://localhost/snapshot/create' \
#     -H  'Accept: application/json' \
#     -H  'Content-Type: application/json' \
#     -d '{
#             "snapshot_type": "Diff",
#             "snapshot_path": "./snapshot/snapshot_file",
#             "mem_file_path": "./snapshot/mem_file"
#     }'



sleep 1s

# resume paused vm

sudo curl --unix-socket /tmp/firecracker.socket -i \
    -X PATCH 'http://localhost/vm' \
    -H 'Accept: application/json' \
    -H 'Content-Type: application/json' \
    -d '{
            "state": "Resumed"
    }'
