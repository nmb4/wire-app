Use lua because bash and powershell both suck.

Tasks that take parameters read them from env vars:
- run: `WIRE_RUN_ARGS="extra args"`
- dev-pair: `WIRE_DEV_SESSION="session-name"`
- bump-version: `WIRE_BUMP_PART=minor` (default: patch)
- upload: `WIRE_DRY_RUN=--dry-run`
- release: `WIRE_BUMP_PART=minor WIRE_DRY_RUN=--dry-run`

---

```lua [name:build]
local ok = os.execute("cargo build -r -p wire-app")
if ok ~= true and ok ~= 0 then os.exit(1) end
```

```lua [name:run, deps:build]
-- Build and run wire-app. Extra args via WIRE_RUN_ARGS.
local IS_WIN = (package.config:sub(1, 1) == "\\") or (os.getenv("OS") or ""):match("Windows") ~= nil
local exe = IS_WIN and "target\\release\\wire-app.exe" or "./target/release/wire-app"
local args = os.getenv("WIRE_RUN_ARGS") or ""
os.execute(exe .. (args ~= "" and (" " .. args) or ""))
```

```lua [name:dev-pair, deps:build]
-- Launch the local three-participant development fixture.
local IS_WIN = (package.config:sub(1, 1) == "\\") or (os.getenv("OS") or ""):match("Windows") ~= nil
local exe = IS_WIN and "target\\release\\wire-app.exe" or "./target/release/wire-app"
local session = os.getenv("WIRE_DEV_SESSION") or ""
os.execute(exe .. " --dev-pair" .. (session ~= "" and (" " .. session) or ""))
```

```lua [name:bump-version]
-- Increment wire-app's patch or minor version in Cargo.toml and Cargo.lock.
-- Usage: WIRE_BUMP_PART=minor upmd up.md --block bump-version --yes
local part = os.getenv("WIRE_BUMP_PART") or "patch"
if part ~= "patch" and part ~= "minor" then
  io.stderr:write("Usage: WIRE_BUMP_PART=[patch|minor] bump-version\n")
  os.exit(1)
end

local function read_file(path)
  local f = io.open(path, "rb")
  if not f then return nil end
  local data = f:read("*a")
  f:close()
  return data
end

local function write_file(path, data)
  local f = io.open(path, "wb")
  if not f then error("Could not write " .. path) end
  f:write(data)
  f:close()
end

local manifest = "wire-app/Cargo.toml"
local lockfile = "Cargo.lock"

local content = read_file(manifest)
if not content then io.stderr:write("Could not read " .. manifest .. "\n"); os.exit(1) end

local lock_content = read_file(lockfile)
if not lock_content then io.stderr:write("Could not read " .. lockfile .. "\n"); os.exit(1) end

-- Find version in [package] section of Cargo.toml.
local pkg_pos = content:find("%[package%]")
if not pkg_pos then io.stderr:write("No [package] section in " .. manifest .. "\n"); os.exit(1) end

local s, e, prefix, maj, min, pat, suffix = content:find(
  '(version%s*=%s*")(%d+)%.(%d+)%.(%d+)(")', pkg_pos)
if not s then io.stderr:write("Could not find version in " .. manifest .. "\n"); os.exit(1) end

local major, minor, patch = tonumber(maj), tonumber(min), tonumber(pat)
local old = string.format("%d.%d.%d", major, minor, patch)

local nmajor, nminor, npatch = major, minor, patch
if part == "minor" then
  nminor = minor + 1
  npatch = 0
else
  npatch = patch + 1
end
local new = string.format("%d.%d.%d", nmajor, nminor, npatch)

-- Update Cargo.toml.
local new_content = content:sub(1, s - 1) .. prefix .. new .. suffix .. content:sub(e + 1)

-- Update Cargo.lock — find the wire-app [[package]] entry.
local ls, le, lprefix, lock_old, lsuffix = lock_content:find(
  '(%[%[package%]%][\r\n]+name%s*=%s*"wire%-app"[\r\n]+version%s*=%s*")([^"]+)(")')
if not ls or lock_old ~= old then
  io.stderr:write("Cargo.lock does not contain wire-app version " .. old .. "\n")
  os.exit(1)
end
local new_lock = lock_content:sub(1, ls - 1) .. lprefix .. new .. lsuffix .. lock_content:sub(le + 1)

write_file(manifest, new_content)
write_file(lockfile, new_lock)

print("Bumped wire-app from " .. old .. " to " .. new .. " (" .. part .. ").")
```

```lua [name:package, deps:build]
-- Copy the release executable to dist and create a versioned zip archive.
local IS_WIN = (package.config:sub(1, 1) == "\\") or (os.getenv("OS") or ""):match("Windows") ~= nil
local SEP = IS_WIN and "\\" or "/"

-- Read version from Cargo.toml.
local f = io.open("wire-app/Cargo.toml", "rb")
if not f then io.stderr:write("Could not read wire-app/Cargo.toml\n"); os.exit(1) end
local toml = f:read("*a")
f:close()
local pkg_pos = toml:find("%[package%]")
local _, _, maj, min, pat = toml:find('version%s*=%s*"(%d+)%.(%d+)%.(%d+)"', pkg_pos)
if not maj then io.stderr:write("Could not read version from Cargo.toml\n"); os.exit(1) end
local version = maj .. "." .. min .. "." .. pat

local exe_name = IS_WIN and "wire-app.exe" or "wire-app"
local exe_src = "target" .. SEP .. "release" .. SEP .. exe_name
local exe_dst = "dist" .. SEP .. exe_name
local zip_path = "dist" .. SEP .. "wire-app-v" .. version .. ".zip"

-- Create dist directory.
os.execute(IS_WIN and "if not exist dist mkdir dist" or "mkdir -p dist")

-- Copy the executable.
local copy_ok = os.execute(IS_WIN
  and string.format('copy /Y "%s" "%s" >NUL', exe_src, exe_dst)
  or string.format('cp "%s" "%s"', exe_src, exe_dst))
if copy_ok ~= true and copy_ok ~= 0 then
  io.stderr:write("Failed to copy executable to dist.\n")
  os.exit(1)
end

-- Create zip archive.
if IS_WIN then
  os.execute(string.format(
    'powershell -NoProfile -Command "Compress-Archive -LiteralPath \'%s\' -DestinationPath \'%s\' -Force"',
    exe_dst, zip_path))
else
  os.execute(string.format('zip -j "%s" "%s"', zip_path, exe_dst))
end

-- Verify zip exists.
local z = io.open(zip_path, "rb")
if not z then io.stderr:write("Failed to create " .. zip_path .. "\n"); os.exit(1) end
z:close()

print("Created " .. exe_dst .. " and " .. zip_path .. ".")
```

```lua [name:upload]
-- Upload the current versioned zip. Set WIRE_DRY_RUN=--dry-run to only print.
local IS_WIN = (package.config:sub(1, 1) == "\\") or (os.getenv("OS") or ""):match("Windows") ~= nil
local SEP = IS_WIN and "\\" or "/"

local dry_run = os.getenv("WIRE_DRY_RUN") or ""
if dry_run ~= "" and dry_run ~= "--dry-run" then
  io.stderr:write("Usage: WIRE_DRY_RUN=--dry-run upload\n")
  os.exit(1)
end

-- Read version from Cargo.toml.
local f = io.open("wire-app/Cargo.toml", "rb")
if not f then io.stderr:write("Could not read wire-app/Cargo.toml\n"); os.exit(1) end
local toml = f:read("*a")
f:close()
local pkg_pos = toml:find("%[package%]")
local _, _, maj, min, pat = toml:find('version%s*=%s*"(%d+)%.(%d+)%.(%d+)"', pkg_pos)
if not maj then io.stderr:write("Could not read version\n"); os.exit(1) end
local version = maj .. "." .. min .. "." .. pat

local zip_path = "dist" .. SEP .. "wire-app-v" .. version .. ".zip"
local upload_url = "https://api.stardive.space/v1/files"

-- Check zip exists.
local z = io.open(zip_path, "rb")
if not z then
  io.stderr:write("Release archive not found: " .. zip_path .. ". Run package first.\n")
  os.exit(1)
end
z:close()

if dry_run == "--dry-run" then
  print("Would upload " .. zip_path .. " to " .. upload_url .. ".")
  os.exit(0)
end

-- Upload with curl.
local tmp = os.tmpname()
print("Uploading " .. zip_path .. "...")
local cmd = string.format(
  'curl --silent --show-error --fail-with-body -F "file=@%s;type=application/zip" -o "%s" "%s"',
  zip_path, tmp, upload_url)
local rc = os.execute(cmd)
if type(rc) == "number" and rc ~= 0 then
  local rf = io.open(tmp, "r")
  if rf then io.stderr:write(rf:read("*a") .. "\n"); rf:close() end
  os.remove(tmp)
  io.stderr:write("Upload failed (exit " .. rc .. ").\n")
  os.exit(1)
elseif rc == nil or rc == false then
  os.remove(tmp)
  io.stderr:write("Upload failed.\n")
  os.exit(1)
end

-- Parse response for file ID.
local rf = io.open(tmp, "r")
local response = rf and rf:read("*a") or ""
if rf then rf:close() end
os.remove(tmp)

local file_id = response:match('"id"%s*:%s*"([^"]+)"')
if not file_id then
  io.stderr:write("Upload completed, but the API response did not contain a file ID: " .. response .. "\n")
  os.exit(1)
end

print("Upload complete.")
print("File ID: " .. file_id)
print("Download: " .. upload_url .. "/" .. file_id)
```

```lua [name:release, deps:"bump-version, package, upload"]
-- Bump, package, and upload.
-- Usage: WIRE_BUMP_PART=minor WIRE_DRY_RUN=--dry-run upmd up.md --block release --yes
print("Release complete.")
```

```lua [name:pack-icon]
-- Pack wire-app/assets/new-icon.png into multi-res icon.ico + icon.png.
-- Requires Pillow:  python -m pip install Pillow
--
--   upmd up.md --block pack-icon --yes
--   WIRE_ICON_ARGS="--rm-bg" upmd up.md --block pack-icon --yes
--   WIRE_ICON_ARGS="--rm-bg --tolerance 12" upmd up.md --block pack-icon --yes
--   WIRE_ICON_ARGS="--rm-bg --color #00FF00" upmd up.md --block pack-icon --yes
--   WIRE_ICON_ARGS="--rm-bg --preview" upmd up.md --block pack-icon --yes
local args = os.getenv("WIRE_ICON_ARGS") or ""
os.execute("python scripts/pack_icon.py" .. (args ~= "" and (" " .. args) or ""))
```
