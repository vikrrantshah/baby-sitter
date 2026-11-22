#!/bin/sh
mode=$1
dir=$2
if [ "$mode" = member-graceful ]; then
    trap 'sleep 2; printf done > "$dir/finished"; exit 0' TERM
    printf ready > "$dir/ready"
    while :; do sleep 1; done
fi
if [ "$mode" = member-stubborn ]; then
    trap '' TERM
    printf ready > "$dir/ready"
    exec sleep 60
fi
/bin/sh "$0" "member-$mode" "$dir" &
wait
