#!/usr/bin/env bash
# Tests of scripts/check-test-messages.sh on sample diffs: each case is a throwaway git repository
# with a base commit and a head commit of lib.rs. Run from anywhere: bash scripts/test_check_test_messages.sh
set -euo pipefail

check="$(cd "$(dirname "$0")" && pwd)/check-test-messages.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fail=0

# case <name> <want exit: 0|1> <base lib.rs> <head lib.rs>
case_run() {
  local name=$1 want=$2 base_src=$3 head_src=$4 dir rc=0
  dir="$tmp/$name"
  mkdir -p "$dir"
  (
    cd "$dir"
    git init -q
    git config user.email test@example.invalid
    git config user.name test
    printf '%s\n' "$base_src" > lib.rs
    git add lib.rs
    git commit -qm base
    base=$(git rev-parse HEAD)
    printf '%s\n' "$head_src" > lib.rs
    git commit -qam head
    "$check" "$base" > out.txt 2>&1 || exit $?
  ) || rc=$?
  # rc 0 or 1 is a verdict; anything else is a usage or git error of the script.
  if [[ $rc -gt 1 ]]; then
    echo "FAIL $name: exit $rc (usage or git error)"
    fail=1
  elif [[ $rc -ne $want ]]; then
    echo "FAIL $name: exit $rc, want $want"
    sed 's/^/    /' "$dir/out.txt"
    fail=1
  else
    echo "ok   $name"
  fi
}

base_leak='fn a() {
    assert_eq!(lines.len(), 1, "{raw}");
}'
clean='fn a() {}'

case_run pre_existing_hit_untouched 0 "$base_leak" "$base_leak
fn b() {
    assert!(ok, \"status {code}\");
}"

case_run added_raw_in_assert_eq 1 "$clean" "fn a() {
    assert_eq!(lines.len(), 1, \"{raw}\");
}"

case_run added_json_on_message_line 1 "$clean" "fn a() {
    assert!(
        ok,
        \"unexpected {json}\"
    );
}"

case_run added_secret_like_name 1 "$clean" "fn a() {
    assert!(ok, \"{api_token}\");
}"

case_run added_canary_prop_assert 1 "$clean" "proptest! {
    fn a(x in 0u8..) {
        prop_assert_ne!(x, 0, \"{canary}\");
    }
}"

case_run modified_line_with_raw 1 "fn a() {
    assert!(ok, \"{raw}\");
}" "fn a() {
    assert!(ok, \"{raw:?}\");
}"

case_run removed_leak_passes 0 "$base_leak" "$clean"

case_run safe_status_message 0 "$clean" "fn a() {
    assert!(ok, \"status {code}\");
}"

case_run raw_in_println_not_assert 0 "$clean" "fn a() {
    println!(\"{raw}\");
}"

case_run statement_boundary_then_format 0 "$clean" "fn a() {
    assert!(ok);
    let msg = format!(\"{token}\");
    drop(msg);
}"

case_run context_is_not_text 0 "$clean" "fn a() {
    assert!(ok, \"{context}\");
}"

if [[ $fail -ne 0 ]]; then
  echo "test_check_test_messages: FAILED"
  exit 1
fi
echo "test_check_test_messages: all cases pass"
