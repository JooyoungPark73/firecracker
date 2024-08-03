sudo timeout 10s perf stat -e task-clock,cycles,instructions,cache-references,cache-misses,minor-faults,major-faults,page-faults,L1-dcache-loads,L1-dcache-load-misses,L1-dcache-stores,L1-dcache-store-misses python server.py --host 127.0.0.1 --port 65432

sudo timeout 10s perf record -e task-clock,cycles,instructions,cache-references,cache-misses,minor-faults,major-faults,page-faults,L1-dcache-loads,L1-dcache-load-misses,L1-dcache-stores python greeter_server_local.py

python client.py --host 127.0.0.1 --port 65432 --name John

while true; do python greeter_client_local.py; sleep 2; done;

while true; do python client.py --host 127.0.0.1 --port 65432 --name John; sleep 2; done;

sudo perf report


# use perf stat than perf record