#!/bin/bash
# Shows Linux-visible RAM from a fresh /proc/meminfo read on every tick: the
# Linux counterpart of tools/windows/Watch-Ram.ps1.
#
# Runs in the guest without root. On a terminal it draws an in-place dashboard:
# memory table, usage bars, memory pressure and page-cache refaults, a rolling
# history of total and in-use RAM, and the last changes in total RAM. Piped or
# redirected, it prints the same layout as plain snapshots. Ctrl+C stops it.
# This observes guest-visible memory, not the size the hypervisor requested.
#
# Usage: watch-ram.sh [-i SECONDS] [-n SAMPLES] [-H HISTORY] [--csv] [--no-clear]
#   -i SECONDS   refresh interval, default 1 (fractions allowed)
#   -n SAMPLES   stop after this many samples; default 0 runs until cancelled
#   -H HISTORY   samples kept for the graph, default 60
#   --csv        one CSV row per sample instead of the dashboard
#   --no-clear   plain snapshots even on a terminal
set -u

INTERVAL=1
SAMPLES=0
HISTORY=60
CSV=0
NOCLEAR=0
while [ $# -gt 0 ]; do
  case "$1" in
    -i) INTERVAL=$2; shift 2 ;;
    -n) SAMPLES=$2; shift 2 ;;
    -H) HISTORY=$2; shift 2 ;;
    --csv) CSV=1; shift ;;
    --no-clear) NOCLEAR=1; shift ;;
    -h|--help) sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done
[ -r /proc/meminfo ] || { echo "this script requires Linux /proc/meminfo" >&2; exit 1; }

WIDTH=78
PLOT_W=60
PLOT_H=5
PAGE_KB=$(( $(getconf PAGESIZE 2>/dev/null || echo 4096) / 1024 ))
CONTROL=/var/lib/rampire-guest/control.json
INTERACTIVE=0
if [ $CSV -eq 0 ] && [ $NOCLEAR -eq 0 ] && [ -t 1 ]; then INTERACTIVE=1; fi

if [ $INTERACTIVE -eq 1 ]; then
  C_RESET=$'\e[0m'; C_BAR=$'\e[1;37;46m'; C_FRAME=$'\e[36m'; C_DIM=$'\e[2;36m'
  C_WHITE=$'\e[1;37m'; C_YELLOW=$'\e[33m'; C_GREEN=$'\e[32m'; C_RED=$'\e[31m'; EOL=$'\e[K'
  FULL='█'; EMPTY='░'; MARK='━'; UP='▲'; DOWN='▼'
  printf '\e[?25l\e[2J'
  trap 'printf "\e[?25h\e[0m\n"' EXIT
  trap 'exit 0' INT TERM
else
  C_RESET=; C_BAR=; C_FRAME=; C_DIM=; C_WHITE=; C_YELLOW=; C_GREEN=; C_RED=; EOL=
  FULL='#'; EMPTY='.'; MARK='T'; UP='+'; DOWN='-'
fi

gib() { printf '%d.%03d' $(( $1 / 1048576 )) $(( ($1 % 1048576) * 1000 / 1048576 )); }
mib() { printf '%d.%01d' $(( $1 / 1024 )) $(( ($1 % 1024) * 10 / 1024 )); }
load_color() { if [ "$1" -ge 90 ]; then printf %s "$C_RED"; elif [ "$1" -ge 70 ]; then printf %s "$C_YELLOW"; else printf %s "$C_GREEN"; fi; }
repeat() { local out=; local i; for ((i = 0; i < $2; i++)); do out+=$1; done; printf %s "$out"; }
line() { printf '%s%s\n' "$1" "$EOL"; }
rule() { line "${C_FRAME}+$(repeat "$1" $((WIDTH - 2)))+${C_RESET}"; }
title() { local text=" $1 "; line "${C_FRAME}+=${C_WHITE}${text}${C_FRAME}$(repeat = $((WIDTH - 3 - ${#text})))+${C_RESET}"; }
row() { # row VISIBLE_TEXT COLOURED_TEXT
  line "${C_FRAME}|${C_RESET} $2$(repeat ' ' $((WIDTH - 4 - ${#1}))) ${C_FRAME}|${C_RESET}"
}
metric() { # metric LABEL NOW SESSION COLOUR
  local plain; plain=$(printf '%-20s%16s    %s' "$1" "$2" "$3")
  row "$plain" "$(printf '%-20s%s%16s%s    %s%s%s' "$1" "$4" "$2" "$C_RESET" "$C_YELLOW" "$3" "$C_RESET")"
}
meter() { # meter LABEL PERCENT DETAIL
  local filled=$(( $2 * 35 / 100 )); [ $filled -gt 35 ] && filled=35
  local plain; plain=$(printf '%-11s%s%s %3d%%  %s' "$1" "$(repeat x $filled)" "$(repeat x $((35 - filled)))" "$2" "$3")
  row "$plain" "$(printf '%-11s%s%s%s%s%s %3d%%%s  %s' "$1" "$(load_color "$2")" "$(repeat "$FULL" $filled)" "$C_DIM" "$(repeat "$EMPTY" $((35 - filled)))" "$(load_color "$2")" "$2" "$C_RESET" "$3")"
}

declare -a H_TOTAL=() H_USED=() H_TIME=() EVENTS=()
previous_total=; baseline=; min_total=; max_total=; min_avail=; peak_used=0
added=0; removed=0; sample=0; previous_refault=; started=$(date +%s)
[ $CSV -eq 1 ] && echo "timestamp,total_kb,available_kb,in_use_kb,file_cache_kb,swap_used_kb,total_delta_kb,psi_some_avg10,refault_kb_per_s,protected_cache_kb"

while [ "$SAMPLES" -eq 0 ] || [ $sample -lt "$SAMPLES" ]; do
  # Never cache the total: hot-add and removal change it between ticks.
  total=0; avail=0; afile=0; ifile=0; swap_total=0; swap_free=0
  while read -r key value _; do
    case "$key" in
      MemTotal:) total=$value ;; MemAvailable:) avail=$value ;;
      'Active(file):') afile=$value ;; 'Inactive(file):') ifile=$value ;;
      SwapTotal:) swap_total=$value ;; SwapFree:) swap_free=$value ;;
    esac
  done < /proc/meminfo
  used=$((total - avail)); cache=$((afile + ifile)); swap_used=$((swap_total - swap_free))
  now=$(date +%T)
  psi=$(awk '/^some/{sub("avg10=","",$2); print $2}' /proc/pressure/memory 2>/dev/null); psi=${psi:-n/a}
  refault=$(awk '$1=="workingset_refault_file"{print $2}' /proc/vmstat 2>/dev/null)
  refault_rate=0
  if [ -n "$refault" ] && [ -n "$previous_refault" ]; then
    refault_rate=$(awk -v d=$((refault - previous_refault)) -v p=$PAGE_KB -v i="$INTERVAL" 'BEGIN{printf "%d", d*p/i}')
  fi
  previous_refault=$refault
  protected=$(grep -o '"protected_cache_bytes":"[0-9]*"' "$CONTROL" 2>/dev/null | grep -o '[0-9]*' | tail -1)
  protected_kb=$(( ${protected:-0} / 1024 ))

  delta=0; [ -n "$previous_total" ] && delta=$((total - previous_total))
  if [ -z "$baseline" ]; then baseline=$total; min_total=$total; max_total=$total; min_avail=$avail; fi
  [ $total -lt $min_total ] && min_total=$total; [ $total -gt $max_total ] && max_total=$total
  [ $avail -lt $min_avail ] && min_avail=$avail; [ $used -gt $peak_used ] && peak_used=$used
  if [ $delta -ne 0 ]; then
    if [ $delta -gt 0 ]; then added=$((added + delta)); else removed=$((removed - delta)); fi
    EVENTS=("$now $delta $total" "${EVENTS[@]:0:2}")
  fi
  previous_total=$total
  sample=$((sample + 1))

  if [ $CSV -eq 1 ]; then
    echo "$(date +%FT%T),$total,$avail,$used,$cache,$swap_used,$delta,$psi,$refault_rate,$protected_kb"
  else
    H_TOTAL+=("$total"); H_USED+=("$used"); H_TIME+=("$now")
    if [ ${#H_TOTAL[@]} -gt "$HISTORY" ]; then H_TOTAL=("${H_TOTAL[@]:1}"); H_USED=("${H_USED[@]:1}"); H_TIME=("${H_TIME[@]:1}"); fi
    used_pct=$((used * 100 / total)); cache_pct=$((cache * 100 / total))
    elapsed=$(( $(date +%s) - started ))
    [ $INTERACTIVE -eq 1 ] && printf '\e[H'
    head=" * VIRTIO-MEM RAM MONITOR  $(hostname)"
    line "${C_BAR}${head}$(repeat ' ' $((WIDTH - ${#head} - 9)))${now} ${C_RESET}"
    line "$(printf ' Session %02d:%02d:%02d   samples %d   refresh %ss' $((elapsed / 3600)) $((elapsed % 3600 / 60)) $((elapsed % 60)) $sample "$INTERVAL")"
    title 'PHYSICAL MEMORY  now / session'
    metric 'Total' "$(gib $total) GiB" "min $(gib $min_total) / max $(gib $max_total) GiB" "$C_WHITE"
    metric 'In use' "$(gib $used) GiB" "peak $(gib $peak_used) GiB" "$(load_color $used_pct)"
    metric 'Available' "$(gib $avail) GiB" "low $(gib $min_avail) GiB" "$C_WHITE"
    metric 'File cache' "$(gib $cache) GiB" "kept by service $(gib $protected_kb) GiB" "$C_WHITE"
    net=$((total - baseline)); net_color=$C_WHITE; [ $net -gt 0 ] && net_color=$C_GREEN; [ $net -lt 0 ] && net_color=$C_RED
    metric 'Net total change' "$(printf '%+d' $((net / 1024))) MiB" "added $(mib $added) / removed $(mib $removed) MiB" "$net_color"
    metric 'Memory pressure' "$psi %" "some avg10; refaults $(mib $refault_rate) MiB/s" "$C_WHITE"
    rule -
    meter 'In use' $used_pct "$(gib $used) / $(gib $total) GiB"
    meter 'File cache' $cache_pct "$(gib $cache) / $(gib $total) GiB"
    rule =

    # Absolute GiB, not percentages, so a change in total stays visible.
    count=${#H_TOTAL[@]}; scale_kb=1048576
    for value in "${H_TOTAL[@]}"; do while [ "$value" -gt $scale_kb ]; do scale_kb=$((scale_kb + 1048576)); done; done
    columns=$count; [ $columns -gt $PLOT_W ] && columns=$PLOT_W
    title "RAM HISTORY  ${MARK}=total  ${FULL}=in use  0..$((scale_kb / 1048576)) GiB"
    for ((y = PLOT_H; y >= 0; y--)); do
      plain=$(printf '%5s |' "$(gib $((scale_kb * y / PLOT_H)) | cut -c1-5)"); colored="${C_DIM}${plain}${C_RESET}"
      for ((column = 0; column < columns; column++)); do
        # Compress the window into columns, keeping the peak in use per bin.
        first=$((column * count / columns)); last=$(((column + 1) * count / columns - 1)); peak=0
        for ((index = first; index <= last; index++)); do [ "${H_USED[index]}" -gt $peak ] && peak=${H_USED[index]}; done
        total_level=$(( (H_TOTAL[last] * PLOT_H + scale_kb / 2) / scale_kb ))
        used_level=$(( (peak * PLOT_H + scale_kb / 2) / scale_kb ))
        if [ $y -eq $total_level ]; then colored+="${C_YELLOW}${MARK}${C_RESET}"
        elif [ $y -le $used_level ]; then colored+="$(load_color $((peak * 100 / H_TOTAL[last])))${FULL}${C_RESET}"
        else colored+=' '; fi
        plain+=x
      done
      row "$plain" "$colored"
    done
    axis=$(printf '      +%s' "$(repeat - $PLOT_W)"); row "$axis" "${C_DIM}${axis}${C_RESET}"
    span=$(printf '       %s oldest%*snewest %s' "${H_TIME[0]}" $((PLOT_W - 30)) '' "${H_TIME[count - 1]}"); row "$span" "$span"
    rule =
    title 'TOTAL RAM CHANGES  newest first'
    for index in 0 1 2; do
      if [ $index -lt ${#EVENTS[@]} ]; then
        read -r at change after <<< "${EVENTS[index]}"
        if [ "$change" -gt 0 ]; then arrow=$UP; color=$C_GREEN; else arrow=$DOWN; color=$C_RED; fi
        plain=$(printf '%s  x %+9d MiB   total %s GiB' "$at" $((change / 1024)) "$(gib "$after")")
        row "$plain" "$(printf '%s  %s%s %+9d MiB%s   total %s GiB' "$at" "$color" "$arrow" $((change / 1024)) "$C_RESET" "$(gib "$after")")"
      elif [ $index -eq 0 ]; then row 'No total-RAM changes observed this session.' "${C_DIM}No total-RAM changes observed this session.${C_RESET}"
      else row '' ''; fi
    done
    rule =
    foot=' Ctrl+C stop | guest-visible RAM, not the size the hypervisor requested'
    line "${C_BAR}${foot}$(repeat ' ' $((WIDTH - ${#foot})))${C_RESET}"
  fi
  if [ "$SAMPLES" -eq 0 ] || [ $sample -lt "$SAMPLES" ]; then sleep "$INTERVAL"; fi
done
