UFFD_SOCKET="/tmp/firecracker-uffd.socket"

# setup custom uffd handler
tmux kill-session -t uffd_handler
sudo rm /tmp/firecracker-uffd.socket
tmux new -s uffd_handler -d
tmux send -t uffd_handler "sudo ./uffd/uffd_valid_count_periodic_handler ${UFFD_SOCKET} ./snapshot/mem_file" ENTER
# tmux send -t uffd_handler "sudo ./uffd/uffd_valid_handler ${UFFD_SOCKET} ./snapshot/mem_file" ENTER

# sudo ./uffd_valid_count_periodic_handler "${UFFD_SOCKET}" ./snapshot/mem_file &
# tmux attach -t uffd_handler

sleep 5s

# load snapshot with uffd
echo "load snapshot"
sudo curl --unix-socket /tmp/firecracker.socket -i \
    -X PUT 'http://localhost/snapshot/load' \
    -H  'Accept: application/json' \
    -H  'Content-Type: application/json' \
    -d '{
            "snapshot_path": "./snapshot/snapshot_file",
            "mem_backend": {
                "backend_path": "/tmp/firecracker-uffd.socket",
                "backend_type": "Uffd"
            },
            "enable_diff_snapshots": true,
            "resume_vm": false
    }'

sleep 5s

# resume snapshot with uffd
echo "resume vm"
sudo curl --unix-socket /tmp/firecracker.socket -i \
    -X PATCH 'http://localhost/vm' \
    -H 'Accept: application/json' \
    -H 'Content-Type: application/json' \
    -d '{
            "state": "Resumed"
    }'
