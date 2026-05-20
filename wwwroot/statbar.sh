#!/usr/bin/env bash

# Ultimate Claude Code Status Line
# Features: Model | Dir | Git (branch/sync/-uall) | Context Bar | Cost | Cache TTL | AI State

# ==========================================
# [BUG FIX] 防止 cat 阻塞导致 exit 卡死
# 只有在存在管道输入时才读取，并加 1 秒超时底线
# ==========================================
if [ ! -t 0 ]; then
    input=$(timeout 1 cat 2>/dev/null || echo "{}")
else
    input="{}"
fi

# -----------------------------------------------------------------------------
# Color Theme configuration
# -----------------------------------------------------------------------------
COLOR="blue"
C_RESET='\033[0m'
C_GRAY='\033[38;5;245m'
C_BAR_EMPTY='\033[38;5;238m'
C_RED='\033[31m'
C_GREEN='\033[32m'

case "$COLOR" in
    orange)   C_ACCENT='\033[38;5;173m' ;;
    blue)     C_ACCENT='\033[38;5;74m' ;;
    teal)     C_ACCENT='\033[38;5;66m' ;;
    green)    C_ACCENT='\033[38;5;71m' ;;
    lavender) C_ACCENT='\033[38;5;139m' ;;
    rose)     C_ACCENT='\033[38;5;132m' ;;
    gold)     C_ACCENT='\033[38;5;136m' ;;
    slate)    C_ACCENT='\033[38;5;60m' ;;
    cyan)     C_ACCENT='\033[38;5;37m' ;;
    *)        C_ACCENT="$C_GRAY" ;;
esac

# -----------------------------------------------------------------------------
# 1. Base Variables (Merged jq for high performance)
# -----------------------------------------------------------------------------
eval "$(echo "$input" | jq -r '
    "model=" + (.model.display_name // .model.id // "?" | @sh),
    "cwd=" + (.cwd // "" | @sh),
    "total_cost=" + (.cost.total_cost_usd // 0 | @sh),
    "session_cost=" + (.cost.session_cost_usd // .cost.total_cost_usd // 0 | @sh),
    "transcript_path=" + (.transcript_path // "" | @sh),
    "max_context=" + (.context_window.context_window_size // 200000 | @sh),
    "pct_fallback=" + (.context_window.used_percentage // 0 | @sh)
' 2>/dev/null || echo "model='?'")"

dir=$(basename "$cwd" 2>/dev/null || echo "?")

# -----------------------------------------------------------------------------
# 2. Fast Git Status (Single Call)
# -----------------------------------------------------------------------------
branch=""
git_status=""
if [[ -n "$cwd" && -d "$cwd/.git" ]]; then
    # 注意：如果 repo 极大，-uall 可能会导致轻微卡顿。若觉得慢可移除 -uall
    git_out=$(git -C "$cwd" --no-optional-locks status --porcelain=v2 --branch 2>/dev/null)
    
    if [[ -n "$git_out" ]]; then
        branch=$(echo "$git_out" | awk '/^# branch.head/ {print $3}')
        
        if [[ -n "$branch" && "$branch" != "(detached)" ]]; then
            file_count=$(echo "$git_out" | grep -v '^#' | wc -l | tr -d ' ')
            sync_status=""
            
            # 解析 branch.ab 获取 ahead/behind (如: +1 -0)
            ab=$(echo "$git_out" | awk '/^# branch.ab/ {print $3, $4}')
            if [[ -n "$ab" ]]; then
                ahead=$(echo "$ab" | awk '{print $1}' | tr -d '+')
                behind=$(echo "$ab" | awk '{print $2}' | tr -d '-')
                if [[ "$ahead" -eq 0 && "$behind" -eq 0 ]]; then sync_status="✓"
                elif [[ "$ahead" -gt 0 && "$behind" -eq 0 ]]; then sync_status="↑${ahead}"
                elif [[ "$ahead" -eq 0 && "$behind" -gt 0 ]]; then sync_status="↓${behind}"
                else sync_status="↑${ahead}↓${behind}"
                fi
            fi
            
            if [[ "$file_count" -eq 0 ]]; then
                git_status=" (${sync_status})"
            elif [[ "$file_count" -eq 1 ]]; then
                single_file=$(echo "$git_out" | grep -v '^#' | head -1 | awk '{print $NF}')
                git_status=" (${single_file}, ${sync_status})"
            else
                git_status=" (${file_count}${sync_status:+, ${sync_status}})"
            fi
        fi
    fi
fi

# -----------------------------------------------------------------------------
# 3. Transcript Analysis Engine: Metrics, AI State, and Context Health
# -----------------------------------------------------------------------------
context_length=0
duration_fmt=""
display_msg=""
current_state="user"
cache_ttl=0
cache_hit=0
cache_miss=0

if [[ -n "$transcript_path" && -f "$transcript_path" ]]; then
    # Inject current unix timestamp to calculate cache TTL
    current_ts=$(date +%s)
    
    eval "$(jq -r --argjson current_ts "$current_ts" '
        def is_unhelpful: startswith("[Request") or . == "";
        def to_secs: if type == "string" then (try fromdateiso8601 catch 0) elif type == "number" then ./1000 else 0 end;

        (.[0].timestamp | to_secs) as $start_sec |
        (.[-1].timestamp | to_secs) as $last_sec |
        (if $start_sec > 0 and $last_sec > $start_sec then $last_sec - $start_sec else 0 end) | floor as $duration |
        
        (if $last_sec > 0 then ($current_ts - $last_sec) else 999 end) as $elapsed |
        (if $elapsed < 300 then 300 - $elapsed else 0 end) | floor as $ttl |
        
        (map(select(.message.usage and .isSidechain != true and .isApiErrorMessage != true)) | last) as $last_usage_msg |
        ($last_usage_msg.message.usage | if . then . else {} end) as $usage |
        (($usage.input_tokens // 0) + ($usage.cache_read_input_tokens // 0) + ($usage.cache_creation_input_tokens // 0)) as $ctx_len |
        ($usage.cache_read_input_tokens // 0) as $hit |
        (($usage.input_tokens // 0) + ($usage.cache_creation_input_tokens // 0)) as $miss |
        
        (.[-1]) as $last_entry |
        (
            if $last_entry.isApiErrorMessage == true then
                { type: "error", msg: "API / Rate Limit Error Triggered" }
            elif $last_entry.type == "assistant" then
                (if ($last_entry.message.content | type == "array") then
                    ( [$last_entry.message.content[] | select(.type == "tool_use")] | last | 
                      if . then { type: "tool", msg: .name } else { type: "assistant", msg: "Thinking..." } end )
                else { type: "assistant", msg: "Thinking..." } end)
            elif $last_entry.type == "tool_result" or ($last_entry.type == "user" and ($last_entry.message.content | type == "array") and (any($last_entry.message.content[]; .type == "tool_result"))) then
                { type: "tool_result", msg: "Processing tool output..." }
            else
                ([.[] | select(.type == "user") | select(.message.content | type == "string" or (type == "array" and any(.[]; .type == "text")))] |
                 reverse | map(.message.content | if type == "string" then . else [.[] | select(.type == "text") | .text] | join(" ") end | gsub("\n"; " ") | gsub(" +"; " ")) |
                 map(select(is_unhelpful | not)) | first // "") | { type: "user", msg: . }
            end
        ) as $state |
         
        "duration_sec=" + ($duration | @sh),
        "context_length=" + ($ctx_len | @sh),
        "cache_hit=" + ($hit | @sh),
        "cache_miss=" + ($miss | @sh),
        "cache_ttl=" + ($ttl | @sh),
        "current_state=" + ($state.type | @sh),
        "display_msg=" + ($state.msg | @sh)
    ' "$transcript_path" 2>/dev/null)"

    if [[ -n "$duration_sec" && "$duration_sec" -gt 0 ]]; then
        h=$((duration_sec / 3600))
        m=$(( (duration_sec % 3600) / 60 ))
        s=$((duration_sec % 60))
        [[ $h -gt 0 ]] && duration_fmt="${h}h${m}m" || duration_fmt="${m}m${s}s"
    fi
fi

# -----------------------------------------------------------------------------
# 4. Context Bar & Hit/Miss Format
# -----------------------------------------------------------------------------
max_k=$((max_context / 1000))
[[ max_k -ge 1000 ]] && max_display="$((max_k / 1000))M" || max_display="${max_k}k"

if [[ "$context_length" -gt 0 ]]; then
    pct=$((context_length * 100 / max_context))
    pct_prefix=""
else 
    pct=$pct_fallback
    [[ "$pct" -eq 0 ]] && pct=$((20000 * 100 / max_context))
    pct_prefix="~"
fi
[[ $pct -gt 100 ]] && pct=100

if [[ $pct -lt 50 ]]; then CTX_COLOR='\033[32m'
elif [[ $pct -lt 80 ]]; then CTX_COLOR='\033[38;5;208m'
else CTX_COLOR='\033[31m'
fi

bar=""
for ((i=0; i<10; i++)); do
    progress=$((pct - i * 10))
    if [[ progress -ge 8 ]]; then bar+="${C_ACCENT}█${C_RESET}"
    elif [[ progress -ge 3 ]]; then bar+="${C_ACCENT}▄${C_RESET}"
    else bar+="${C_BAR_EMPTY}░${C_RESET}"
    fi
done
ctx="${bar} ${CTX_COLOR}${pct_prefix}${pct}%${C_RESET} of ${max_display}"

# Format Cache Stats (e.g. 12500 -> 12k)
fmt_tokens() {
    local val=$1
    if [[ $val -ge 1000 ]]; then echo "$((val / 1000))k"
    else echo "$val"
    fi
}
hit_k=$(fmt_tokens "$cache_hit")
miss_k=$(fmt_tokens "$cache_miss")

# -----------------------------------------------------------------------------
# 5. Cost Formatting
# -----------------------------------------------------------------------------
format_cost() {
    awk -v val="$1" 'BEGIN {
        if (val == 0) print "0.00"
        else if (val < 0.01) print "<0.01"
        else printf "%.2f", val
    }'
}

cost_session_str=$(format_cost "$session_cost")
cost_total_str=$(format_cost "$total_cost")

COST_COLOR=$C_RESET
if awk -v val="$session_cost" 'BEGIN {exit !(val >= 1.00)}'; then
    COST_COLOR='\033[31m'
fi

# -----------------------------------------------------------------------------
# 6. Output Construction (Line 1: Status, Line 2: AI State / Context)
# -----------------------------------------------------------------------------
# Line 1: Primary Metrics
line1="${C_ACCENT}${model}${C_RESET} | 📁 ${dir}"
[[ -n "$branch" ]] && line1+=" | 🔀 ${branch}${git_status}"
line1+=" | ${ctx}"
[[ -n "$duration_fmt" ]] && line1+=" | ⏱ ${duration_fmt}"
line1+=" | 💰 ${COST_COLOR}\$${cost_session_str}${C_RESET} / \$${cost_total_str}"

printf '%b\n' "$line1"

# Line 2: Action State, Cache Info & Output Message
if [[ -n "$display_msg" ]]; then
    # Estimate plain text length to determine truncation point
    plain_output="${model} | 📁 ${dir} | 🔀 ${branch}${git_status} | xxxxxxxxxx ${pct}% of ${max_display} | ⏱ 00m00s | 💰 $99.99 / 999.99"
    max_len=${#plain_output}

    line2_prefix=""
    
    if [[ "$current_state" == "user" ]]; then
        # 1. Format Cache TTL Timer
        if [[ "$cache_ttl" -gt 0 ]]; then
            m=$((cache_ttl / 60))
            s=$((cache_ttl % 60))
            line2_prefix+="${C_GREEN}[⚡ $(printf "%02d:%02d" $m $s)]${C_RESET} "
        else
            line2_prefix+="${C_GRAY}[⚡ Miss]${C_RESET} "
        fi
        
        # 2. Format Context Usage Breakdown
        if [[ "$cache_hit" -gt 0 || "$cache_miss" -gt 0 ]]; then
            line2_prefix+="${C_GRAY}(Hit:${hit_k} Miss:${miss_k})${C_RESET} 💬 "
        else
            line2_prefix+="💬 "
        fi
        
        avail_len=$((max_len - 30))
        [[ $avail_len -lt 20 ]] && avail_len=20
        
        if [[ ${#display_msg} -gt $avail_len ]]; then
            echo -e "${line2_prefix}${display_msg:0:$((avail_len - 3))}..."
        else
            echo -e "${line2_prefix}${display_msg}"
        fi
        
    elif [[ "$current_state" == "tool" ]]; then
        echo -e "🛠️  ${C_ACCENT}Running Tool:${C_RESET} ${display_msg}"
    elif [[ "$current_state" == "tool_result" ]]; then
        echo -e "⚙️  ${C_GRAY}${display_msg}${C_RESET}"
    elif [[ "$current_state" == "assistant" ]]; then
        echo -e "⏳ ${C_ACCENT}${display_msg}${C_RESET}"
    elif [[ "$current_state" == "error" ]]; then
        echo -e "⚠️  ${C_RED}${display_msg}${C_RESET}"
    fi
fi

# [BUG FIX] 保证脚本绝对会成功退出，不阻塞主程序
exit 0