sudo curl --unix-socket "${API_SOCKET}" -i \
-X PATCH 'http://localhost/vm' \
    -H 'Accept: application/json' \
    -H 'Content-Type: application/json' \
    -d '{
            "state": "Paused"
    }'


sudo rm ./snapshot/snapshot_file
sudo rm ./snapshot/mem_file
# Capture full snapshot
sudo curl --unix-socket "${API_SOCKET}" -i \
    -X PUT 'http://localhost/snapshot/create' \
    -H  'Accept: application/json' \
    -H  'Content-Type: application/json' \
    -d '{
            "snapshot_type": "Full",
            "snapshot_path": "./snapshot/snapshot_file",
            "mem_file_path": "./snapshot/mem_file"
    }'

sudo curl --unix-socket "${API_SOCKET}" -i \
    -X PATCH 'http://localhost/vm' \
    -H 'Accept: application/json' \
    -H 'Content-Type: application/json' \
    -d '{
            "state": "Resumed"
    }'

