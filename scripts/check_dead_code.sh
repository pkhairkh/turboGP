#!/bin/bash
# Wave 1: Check that no dead/unreachable modules exist in src/.
# Scans for modules declared in mod.rs but never referenced from
# the production execution path (QueryEngine::execute).
#
# This is a heuristic check — cargo-udeps catches unused dependencies,
# this script catches unreachable modules. Both are needed.
#
# Exit 0 = pass, Exit 1 = dead code found.

set -e

echo "[dead-code-check] Scanning src/ for unreachable modules..."

# Get all .rs files in src/
FILES=$(find src/ -name '*.rs' | sort)

DEAD_COUNT=0

# For each module file, check if its primary type/function is referenced
# anywhere in src/ outside its own file and outside #[cfg(test)].
for file in $FILES; do
    # Skip mod.rs files (they're just re-exports)
    if [[ "$file" == *"/mod.rs" ]]; then
        continue
    fi

    # Extract the module name from the file path
    mod_name=$(basename "$file" .rs)

    # Get the primary type name (PascalCase version of mod_name)
    # e.g., "flat_hash_table" -> "FlatHashTable"
    type_name=$(echo "$mod_name" | awk -F_ '{for(i=1;i<=NF;i++) $i=toupper(substr($i,1,1))substr($i,2)}1' OFS='')

    # Check if this type is referenced anywhere in src/ except its own file
    # and except test modules
    refs=$(grep -rl "$type_name" src/ 2>/dev/null | grep -v "$file" | grep -v "#\[cfg(test)\]" | head -1 || true)

    if [ -z "$refs" ]; then
        # Also check if the module is `use`d anywhere
        mod_path=$(echo "$file" | sed 's|src/||; s|/|::|g; s|\.rs$||')
        use_refs=$(grep -rl "use.*$mod_name" src/ 2>/dev/null | grep -v "$file" | grep -v "mod.rs" | head -1 || true)

        if [ -z "$use_refs" ]; then
            echo "  DEAD: $file (type $type_name not referenced outside own file)"
            DEAD_COUNT=$((DEAD_COUNT + 1))
        fi
    fi
done

if [ "$DEAD_COUNT" -gt 0 ]; then
    echo ""
    echo "ERROR: Found $DEAD_COUNT potentially dead modules in src/."
    echo "Either wire them to QueryEngine::execute or delete them."
    exit 1
fi

echo "OK: No dead modules detected."
exit 0
