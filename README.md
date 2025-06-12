# running

see https://github.com/xetdata/nfsserve/tree/main

## macos
```sh
# change to actual IP or hostname
BOARD=10.0.0.2
mkdir demo
mount_nfs -o nolocks,vers=3,tcp,rsize=131072,actimeo=120,port=11111,mountport=11111 $BOARD:/ demo
# or maybe
mount -t nfs -o nolocks,vers=3,tcp,port=11111,mountport=11111,soft $BOARD:/ demo
```