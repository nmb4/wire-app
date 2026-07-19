#!/usr/bin/env lua
--[[
  kanbn.lua — thin CLI for the Kan REST API (https://kan.bn/api/v1)

  Auth:  KANBN_API_KEY=... in the environment or a nearby .env file
  HTTP:  prefers curl, falls back to wget
  Deps:  none (pure Lua JSON codec; no third-party libs)

  Usage:
    kanbn.lua me
    kanbn.lua workspaces
    kanbn.lua workspace <id-or-slug>
    kanbn.lua boards <workspacePublicId>
    kanbn.lua board <boardPublicId>
    kanbn.lua card <cardPublicId>
    kanbn.lua search <workspacePublicId> <query> [--limit N]
    kanbn.lua find-workspace <name>       # case-insensitive name match
    kanbn.lua find-board <workspaceId> <name>
    kanbn.lua explore-board <boardPublicId>   # human summary of lists/cards
    kanbn.lua backup-board <boardPublicId> [out-dir]
    kanbn.lua restore-board <backup-path> --workspace <id> [--name NAME]
    kanbn.lua get <path> [query=value ...]    # raw GET, path starts with /
    kanbn.lua request <METHOD> <path> [body]  # raw request (body as JSON string)

  Global flags (anywhere):
    --raw          print raw response body (no pretty-print)
    --base URL     override API base (default https://kan.bn/api/v1)
    --quiet        suppress non-data stderr noise
    --timeout SEC  total request timeout in seconds (default 60; 0 = none)
    --connect-timeout SEC
                   TCP connect timeout in seconds (default 15; 0 = none)

  Env (environment variables take precedence over .env values):
    KANBN_API_KEY, KANBN_TIMEOUT, KANBN_CONNECT_TIMEOUT

  backup-board flags:
    --no-attachments   skip downloading attachment files (metadata only)
    --skip-activities  omit raw activity history (comments still kept)

  restore-board flags:
    --workspace ID     target workspace public id (required unless in backup)
    --name NAME        board name override
    --dry-run          print plan, do not write
    --skip-attachments skip uploading attachment files
    --skip-comments    skip recreating comments
]]

local BASE_DEFAULT = "https://kan.bn/api/v1"
local VERSION = "0.2.1"
local BACKUP_FORMAT = "kanbn-board-backup"
local BACKUP_VERSION = 1
local TIMEOUT_DEFAULT = 60        -- total seconds per HTTP call
local CONNECT_TIMEOUT_DEFAULT = 15

--------------------------------------------------------------------
-- tiny helpers
--------------------------------------------------------------------

local function die(msg, code)
  io.stderr:write("kanbn: " .. tostring(msg) .. "\n")
  os.exit(code or 1)
end

local function trim(s)
  return (tostring(s):gsub("^%s+", ""):gsub("%s+$", ""))
end

-- Platform detection: Windows uses cmd.exe for os.execute/io.popen,
-- which needs different quoting, redirection, and shell builtins.
local IS_WINDOWS = (package.config:sub(1, 1) == "\\")
  or (os.getenv("OS") or ""):match("Windows") ~= nil

-- Redirect-to-null target for the current shell.
local NULL_DEVICE = IS_WINDOWS and "NUL" or "/dev/null"

local function shell_quote(s)
  s = tostring(s)
  if IS_WINDOWS then
    -- cmd.exe: wrap in double quotes; escape embedded quotes by doubling.
    -- Do NOT escape "%": on the cmd command line (unlike inside a .bat
    -- file) "%%" stays literal, which would break curl's -w "%{http_code}".
    s = s:gsub('"', '""')
    return '"' .. s .. '"'
  end
  if s == "" then return "''" end
  return "'" .. s:gsub("'", "'\\''") .. "'"
end

local function command_exists(name)
  local ok
  if IS_WINDOWS then
    -- `where` returns 0 if the executable is found on PATH.
    ok = os.execute("where " .. shell_quote(name) .. " >NUL 2>&1")
  else
    -- `command -v` works in sh; suppress output
    ok = os.execute("command -v " .. shell_quote(name) .. " >/dev/null 2>&1")
  end
  -- Lua 5.1: true/nil; 5.2+/LuaJIT: true or exit status integer
  if ok == true or ok == 0 then return true end
  return false
end

local function tmpname()
  local t = os.tmpname()
  if IS_WINDOWS then
    -- Some Lua builds return a bare root path (e.g. "\s3a.") that is not
    -- writable. If there is no drive/dir component, relocate into TEMP.
    if not t:match("^%a:[\\/]") then
      local tmp = os.getenv("TEMP") or os.getenv("TMP") or "."
      t = t:gsub("^[\\/]+", "")
      t = tmp:gsub("[\\/]+$", "") .. "\\" .. t
    end
  end
  return t
end

local function read_file(path)
  local f, err = io.open(path, "rb")
  if not f then return nil, err end
  local data = f:read("*a")
  f:close()
  return data
end

-- Load simple KEY=VALUE entries from .env without changing the process
-- environment. Prefer the current directory, then the repository root next
-- to this script so `lua scripts/kanbn.lua ...` works from either location.
local function load_dotenv()
  local paths = { ".env" }
  local script = rawget(_G, "arg") and arg[0] or nil
  local script_dir = script and script:match("^(.*)[/\\][^/\\]+$")
  if script_dir and script_dir ~= "" then
    paths[#paths + 1] = script_dir .. "/../.env"
  end

  for _, path in ipairs(paths) do
    local contents = read_file(path)
    if contents then
      local values = {}
      for line in contents:gmatch("[^\r\n]+") do
        line = line:gsub("^%s*export%s+", "")
        local key, value = line:match("^%s*([%w_]+)%s*=%s*(.-)%s*$")
        if key and value and value:sub(1, 1) ~= "#" then
          if (value:sub(1, 1) == '"' and value:sub(-1) == '"')
              or (value:sub(1, 1) == "'" and value:sub(-1) == "'") then
            value = value:sub(2, -2)
          end
          values[key] = value
        end
      end
      return values
    end
  end
  return {}
end

local DOTENV = load_dotenv()

local function env_or_dotenv(name)
  local value = os.getenv(name)
  if value and trim(value) ~= "" then return value end
  return DOTENV[name]
end

local function write_file(path, data)
  local f, err = io.open(path, "wb")
  if not f then return nil, err end
  f:write(data or "")
  f:close()
  return true
end

local function url_encode(s)
  s = tostring(s)
  return (s:gsub("([^%w%-_%.~])", function(c)
    return string.format("%%%02X", string.byte(c))
  end))
end

--------------------------------------------------------------------
-- pure Lua JSON (decode + encode) — sufficient for API payloads
--------------------------------------------------------------------

local json = {}

local function json_error(msg, i)
  error(string.format("json: %s at position %d", msg, i or 0), 0)
end

function json.decode(str)
  if type(str) ~= "string" then
    error("json.decode: expected string", 2)
  end
  local i = 1
  local n = #str

  local function peek()
    return str:sub(i, i)
  end

  local function skip_ws()
    local s, e = str:find("^[ \t\r\n]+", i)
    if s then i = e + 1 end
  end

  local parse_value

  local function parse_string()
    if peek() ~= '"' then json_error("expected string", i) end
    i = i + 1
    local out = {}
    while i <= n do
      local c = str:sub(i, i)
      if c == '"' then
        i = i + 1
        return table.concat(out)
      elseif c == "\\" then
        local nch = str:sub(i + 1, i + 1)
        local map = { ['"'] = '"', ["\\"] = "\\", ["/"] = "/", b = "\b", f = "\f", n = "\n", r = "\r", t = "\t" }
        if map[nch] then
          out[#out + 1] = map[nch]
          i = i + 2
        elseif nch == "u" then
          local hex = str:sub(i + 2, i + 5)
          if not hex:match("^[0-9a-fA-F]+$") or #hex < 4 then
            json_error("invalid unicode escape", i)
          end
          local code = tonumber(hex, 16)
          if code < 0x80 then
            out[#out + 1] = string.char(code)
          elseif code < 0x800 then
            out[#out + 1] = string.char(0xC0 + math.floor(code / 0x40), 0x80 + (code % 0x40))
          else
            out[#out + 1] = string.char(
              0xE0 + math.floor(code / 0x1000),
              0x80 + (math.floor(code / 0x40) % 0x40),
              0x80 + (code % 0x40)
            )
          end
          i = i + 6
        else
          json_error("invalid escape", i)
        end
      else
        -- read a run of non-special bytes
        local s, e = str:find('[^"\\]+', i)
        if not s or s ~= i then
          out[#out + 1] = c
          i = i + 1
        else
          out[#out + 1] = str:sub(s, e)
          i = e + 1
        end
      end
    end
    json_error("unterminated string", i)
  end

  local function parse_number()
    local s, e = str:find("^-?%d+%.?%d*[eE][+%-]?%d+", i)
    if not s then s, e = str:find("^-?%d+%.%d+", i) end
    if not s then s, e = str:find("^-?%d+", i) end
    if not s or s ~= i then json_error("invalid number", i) end
    local num = tonumber(str:sub(s, e))
    i = e + 1
    return num
  end

  local function parse_array()
    i = i + 1 -- [
    skip_ws()
    local arr = {}
    if peek() == "]" then
      i = i + 1
      return arr
    end
    while true do
      skip_ws()
      arr[#arr + 1] = parse_value()
      skip_ws()
      local c = peek()
      if c == "]" then
        i = i + 1
        return arr
      elseif c == "," then
        i = i + 1
      else
        json_error("expected , or ] in array", i)
      end
    end
  end

  local function parse_object()
    i = i + 1 -- {
    skip_ws()
    local obj = {}
    if peek() == "}" then
      i = i + 1
      return obj
    end
    while true do
      skip_ws()
      if peek() ~= '"' then json_error("expected object key", i) end
      local key = parse_string()
      skip_ws()
      if peek() ~= ":" then json_error("expected : after key", i) end
      i = i + 1
      skip_ws()
      obj[key] = parse_value()
      skip_ws()
      local c = peek()
      if c == "}" then
        i = i + 1
        return obj
      elseif c == "," then
        i = i + 1
      else
        json_error("expected , or } in object", i)
      end
    end
  end

  parse_value = function()
    skip_ws()
    local c = peek()
    if c == '"' then
      return parse_string()
    elseif c == "{" then
      return parse_object()
    elseif c == "[" then
      return parse_array()
    elseif c == "t" and str:sub(i, i + 3) == "true" then
      i = i + 4
      return true
    elseif c == "f" and str:sub(i, i + 4) == "false" then
      i = i + 5
      return false
    elseif c == "n" and str:sub(i, i + 3) == "null" then
      i = i + 4
      return nil
    elseif c == "-" or c:match("%d") then
      return parse_number()
    else
      json_error("unexpected character '" .. c .. "'", i)
    end
  end

  local ok, val = pcall(parse_value)
  if not ok then error(val, 0) end
  skip_ws()
  if i <= n then
    -- allow trailing whitespace only
    json_error("trailing garbage", i)
  end
  return val
end

local function is_array(t)
  if type(t) ~= "table" then return false end
  local count = 0
  local max = 0
  for k, _ in pairs(t) do
    if type(k) ~= "number" or k < 1 or k % 1 ~= 0 then return false end
    count = count + 1
    if k > max then max = k end
  end
  return max == count
end

local function encode_string(s)
  local map = {
    ['"'] = '\\"',
    ["\\"] = "\\\\",
    ["\b"] = "\\b",
    ["\f"] = "\\f",
    ["\n"] = "\\n",
    ["\r"] = "\\r",
    ["\t"] = "\\t",
  }
  return '"' .. s:gsub('["\\\b\f\n\r\t]', map):gsub("[%z\1-\31]", function(c)
    return string.format("\\u%04x", string.byte(c))
  end) .. '"'
end

function json.encode(val, pretty, indent)
  indent = indent or 0
  local pad = pretty and string.rep("  ", indent) or ""
  local pad1 = pretty and string.rep("  ", indent + 1) or ""
  local nl = pretty and "\n" or ""
  local sp = pretty and " " or ""

  local t = type(val)
  if val == nil then
    return "null"
  elseif t == "boolean" then
    return val and "true" or "false"
  elseif t == "number" then
    if val ~= val or val == math.huge or val == -math.huge then
      error("json.encode: invalid number")
    end
    return string.format("%.14g", val)
  elseif t == "string" then
    return encode_string(val)
  elseif t == "table" then
    if is_array(val) then
      if #val == 0 then return "[]" end
      local parts = {}
      for i = 1, #val do
        parts[i] = pad1 .. json.encode(val[i], pretty, indent + 1)
      end
      return "[" .. nl .. table.concat(parts, "," .. nl) .. nl .. pad .. "]"
    else
      local keys = {}
      for k in pairs(val) do
        if type(k) == "string" then keys[#keys + 1] = k end
      end
      table.sort(keys)
      if #keys == 0 then return "{}" end
      local parts = {}
      for _, k in ipairs(keys) do
        parts[#parts + 1] = pad1
          .. encode_string(k)
          .. ":"
          .. sp
          .. json.encode(val[k], pretty, indent + 1)
      end
      return "{" .. nl .. table.concat(parts, "," .. nl) .. nl .. pad .. "}"
    end
  else
    error("json.encode: unsupported type " .. t)
  end
end

--------------------------------------------------------------------
-- HTTP (curl preferred, wget fallback)
--------------------------------------------------------------------

local HTTP = {
  tool = nil, -- "curl" | "wget"
  -- seconds; 0 disables. Overridden by opts / env / flags before use.
  timeout = TIMEOUT_DEFAULT,
  connect_timeout = CONNECT_TIMEOUT_DEFAULT,
}

local function parse_timeout_seconds(v, flag_name)
  if v == nil or v == "" then return nil end
  local n = tonumber(v)
  if not n or n < 0 or n ~= math.floor(n) then
    die(flag_name .. " must be a non-negative integer (seconds), got: " .. tostring(v))
  end
  return n
end

local function apply_timeout_defaults_from_env()
  local t = parse_timeout_seconds(env_or_dotenv("KANBN_TIMEOUT"), "KANBN_TIMEOUT")
  local c = parse_timeout_seconds(env_or_dotenv("KANBN_CONNECT_TIMEOUT"), "KANBN_CONNECT_TIMEOUT")
  if t ~= nil then HTTP.timeout = t end
  if c ~= nil then HTTP.connect_timeout = c end
end

apply_timeout_defaults_from_env()

-- Normalize os.execute return across Lua 5.1 / 5.2+ / LuaJIT.
-- Returns process exit code (0 = success), or 128+signal when killed by signal.
local function exec_exit_code(a, b, c)
  if a == true then return 0 end
  if a == nil or a == false then
    if type(c) == "number" then
      if b == "signal" then return 128 + c end
      return c
    end
    return 1
  end
  if type(a) == "number" then
    -- Lua 5.1: raw wait status. Exited → code in high byte.
    if a == 0 then return 0 end
    if a > 0 then
      local code = math.floor(a / 256)
      local sig = a % 256
      if sig ~= 0 then return 128 + sig end
      return code
    end
    return a
  end
  return 1
end

local function run_cmd(cmd)
  local a, b, c = os.execute(cmd)
  return exec_exit_code(a, b, c)
end

-- curl: 28 = timeout; wget: 4 = network failure (includes timeout)
local function is_timeout_exit(tool, code)
  if tool == "curl" then return code == 28 end
  if tool == "wget" then return code == 4 end
  return false
end

function HTTP.detect()
  if HTTP.tool then return HTTP.tool end
  if command_exists("curl") then
    HTTP.tool = "curl"
  elseif command_exists("wget") then
    HTTP.tool = "wget"
  else
    die("neither curl nor wget found in PATH")
  end
  return HTTP.tool
end

-- Append curl/wget timeout flags. timeout_override may be a number (total secs).
local function append_timeout_flags(parts, tool, timeout_override)
  local total = timeout_override
  if total == nil then total = HTTP.timeout end
  local connect = HTTP.connect_timeout

  if tool == "curl" then
    if connect and connect > 0 then
      parts[#parts + 1] = "--connect-timeout"
      parts[#parts + 1] = tostring(connect)
    end
    if total and total > 0 then
      parts[#parts + 1] = "--max-time"
      parts[#parts + 1] = tostring(total)
    end
  else
    -- wget: --timeout sets dns/connect/read when specific flags omitted
    if connect and connect > 0 then
      parts[#parts + 1] = "--connect-timeout=" .. tostring(connect)
    end
    if total and total > 0 then
      parts[#parts + 1] = "--read-timeout=" .. tostring(total)
      -- also cap overall with --timeout for dns + connect + read budget
      parts[#parts + 1] = "--timeout=" .. tostring(total)
    end
  end
end

-- returns body (string), status (number), err (string|nil)
-- err is "timeout" or a short failure reason when no HTTP status is available.
function HTTP.request(method, url, headers, body, timeout_override)
  local tool = HTTP.detect()
  local body_file = tmpname()
  local out_file = tmpname()
  local code_file = tmpname()
  local err_file = tmpname()
  local hdr_args = {}

  headers = headers or {}
  for k, v in pairs(headers) do
    hdr_args[#hdr_args + 1] = { k, v }
  end

  local cmd
  if tool == "curl" then
    local parts = {
      "curl",
      "-sS",
      "-X", shell_quote(method),
      "-o", shell_quote(out_file),
      "-w", shell_quote("%{http_code}"),
    }
    append_timeout_flags(parts, "curl", timeout_override)
    for _, hv in ipairs(hdr_args) do
      parts[#parts + 1] = "-H"
      parts[#parts + 1] = shell_quote(hv[1] .. ": " .. hv[2])
    end
    if body ~= nil then
      write_file(body_file, body)
      parts[#parts + 1] = "--data-binary"
      parts[#parts + 1] = "@" .. shell_quote(body_file)
    end
    parts[#parts + 1] = shell_quote(url)
    -- status code → code_file; curl errors → err_file
    cmd = table.concat(parts, " ")
      .. " > " .. shell_quote(code_file)
      .. " 2> " .. shell_quote(err_file)
  else
    local parts = {
      "wget",
      "-q",
      "-O", shell_quote(out_file),
      "--method=" .. shell_quote(method),
      "--server-response",
    }
    append_timeout_flags(parts, "wget", timeout_override)
    for _, hv in ipairs(hdr_args) do
      parts[#parts + 1] = "--header=" .. shell_quote(hv[1] .. ": " .. hv[2])
    end
    if body ~= nil then
      write_file(body_file, body)
      parts[#parts + 1] = "--body-file=" .. shell_quote(body_file)
    end
    parts[#parts + 1] = shell_quote(url)
    -- headers + errors on stderr
    cmd = table.concat(parts, " ") .. " 2>" .. shell_quote(code_file)
  end

  local exit_code = run_cmd(cmd)
  local raw_out = read_file(out_file) or ""
  local meta = read_file(code_file) or ""
  local err_out = ""
  if tool == "curl" then
    err_out = read_file(err_file) or ""
  end
  os.remove(body_file)
  os.remove(out_file)
  os.remove(code_file)
  os.remove(err_file)

  if is_timeout_exit(tool, exit_code) then
    local t = timeout_override
    if t == nil then t = HTTP.timeout end
    local msg = "request timed out"
    if t and t > 0 then
      msg = string.format("request timed out after %ds", t)
    end
    return raw_out, 0, msg
  end

  local status
  if tool == "curl" then
    status = tonumber(trim(meta))
    if (not status or status == 0) and exit_code ~= 0 then
      local detail = trim(err_out)
      if detail == "" then
        detail = string.format("curl exit %d", exit_code)
      end
      return raw_out, 0, detail
    end
  else
    local last
    for line in meta:gmatch("[^\r\n]+") do
      local code = line:match("^%s*HTTP/%S+%s+(%d+)")
      if code then last = tonumber(code) end
    end
    status = last
    if not status then
      if exit_code == 0 then
        status = 200
      else
        return raw_out, 0, string.format("wget exit %d", exit_code)
      end
    end
  end

  return raw_out, status or 0, nil
end

-- Download a URL to a local file (no Kan auth; used for attachment URLs).
-- timeout_override optional (attachments may need longer).
function HTTP.download(url, dest_path, timeout_override)
  local tool = HTTP.detect()
  local err_file = tmpname()
  local parts
  if tool == "curl" then
    parts = {
      "curl", "-sS", "-L",
      "-o", shell_quote(dest_path),
    }
    append_timeout_flags(parts, "curl", timeout_override)
    parts[#parts + 1] = shell_quote(url)
  else
    parts = {
      "wget", "-q",
      "-O", shell_quote(dest_path),
    }
    append_timeout_flags(parts, "wget", timeout_override)
    parts[#parts + 1] = shell_quote(url)
  end
  local cmd = table.concat(parts, " ") .. " 2>" .. shell_quote(err_file)
  local exit_code = run_cmd(cmd)
  local err_out = trim(read_file(err_file) or "")
  os.remove(err_file)

  if is_timeout_exit(tool, exit_code) then
    local t = timeout_override
    if t == nil then t = HTTP.timeout end
    local msg = "download timed out"
    if t and t > 0 then
      msg = string.format("download timed out after %ds", t)
    end
    return nil, msg
  end
  if exit_code ~= 0 then
    if err_out ~= "" then return nil, err_out end
    return nil, string.format("download failed (exit %d)", exit_code)
  end
  local f = io.open(dest_path, "rb")
  if not f then return nil, "download produced no file" end
  f:close()
  return true
end

-- Upload a local file with an arbitrary method (e.g. PUT to a presigned URL).
-- Returns status (number), err (string|nil).
function HTTP.upload_file(method, url, headers, file_path, timeout_override)
  local tool = HTTP.detect()
  local code_file = tmpname()
  local err_file = tmpname()
  local cmd
  if tool == "curl" then
    local parts = {
      "curl", "-sS",
      "-X", shell_quote(method),
      "-o", NULL_DEVICE,
      "-w", shell_quote("%{http_code}"),
      "-T", shell_quote(file_path),
    }
    append_timeout_flags(parts, "curl", timeout_override)
    for k, v in pairs(headers or {}) do
      parts[#parts + 1] = "-H"
      parts[#parts + 1] = shell_quote(k .. ": " .. v)
    end
    parts[#parts + 1] = shell_quote(url)
    cmd = table.concat(parts, " ")
      .. " > " .. shell_quote(code_file)
      .. " 2> " .. shell_quote(err_file)
  else
    local parts = {
      "wget", "-q",
      "-O", NULL_DEVICE,
      "--method=" .. shell_quote(method),
      "--body-file=" .. shell_quote(file_path),
      "--server-response",
    }
    append_timeout_flags(parts, "wget", timeout_override)
    for k, v in pairs(headers or {}) do
      parts[#parts + 1] = "--header=" .. shell_quote(k .. ": " .. v)
    end
    parts[#parts + 1] = shell_quote(url)
    cmd = table.concat(parts, " ") .. " 2>" .. shell_quote(code_file)
  end
  local exit_code = run_cmd(cmd)
  local meta = read_file(code_file) or ""
  local err_out = ""
  if tool == "curl" then
    err_out = trim(read_file(err_file) or "")
  end
  os.remove(code_file)
  os.remove(err_file)

  if is_timeout_exit(tool, exit_code) then
    local t = timeout_override
    if t == nil then t = HTTP.timeout end
    local msg = "upload timed out"
    if t and t > 0 then
      msg = string.format("upload timed out after %ds", t)
    end
    return 0, msg
  end

  if tool == "curl" then
    local status = tonumber(trim(meta))
    if (not status or status == 0) and exit_code ~= 0 then
      if err_out ~= "" then return 0, err_out end
      return 0, string.format("curl exit %d", exit_code)
    end
    return status or 0, nil
  end

  local last = 0
  for line in meta:gmatch("[^\r\n]+") do
    local code = line:match("^%s*HTTP/%S+%s+(%d+)")
    if code then last = tonumber(code) end
  end
  if last == 0 and exit_code ~= 0 then
    return 0, string.format("wget exit %d", exit_code)
  end
  return last, nil
end

--------------------------------------------------------------------
-- Kan client
--------------------------------------------------------------------

local opts = {
  raw = false,
  quiet = false,
  base = BASE_DEFAULT,
  api_key = env_or_dotenv("KANBN_API_KEY"),
  timeout = HTTP.timeout,
  connect_timeout = HTTP.connect_timeout,
  -- command flags
  workspace = nil,
  name = nil,
  dry_run = false,
  no_attachments = false,
  skip_attachments = false,
  skip_comments = false,
  skip_activities = false,
  limit = nil,
  body_file = nil,
}

local function log(msg)
  if not opts.quiet then
    io.stderr:write(msg .. "\n")
  end
end

local function require_key()
  -- Strip surrounding single/double quotes so keys stored as "kan_..." in
  -- .env still authenticate (naive $(split '=')[1] parsing keeps the quotes).
  if opts.api_key then
    opts.api_key = opts.api_key:gsub('^["\']', ''):gsub('["\']$', '')
  end
  if not opts.api_key or opts.api_key == "" then
    die("KANBN_API_KEY is not set. Create a key at https://kan.bn/settings")
  end
end

local function build_url(path, query)
  if path:sub(1, 1) ~= "/" then path = "/" .. path end
  local url = opts.base:gsub("/+$", "") .. path
  if query and next(query) then
    local parts = {}
    for k, v in pairs(query) do
      if v ~= nil then
        parts[#parts + 1] = url_encode(k) .. "=" .. url_encode(tostring(v))
      end
    end
    table.sort(parts)
    if #parts > 0 then url = url .. "?" .. table.concat(parts, "&") end
  end
  return url
end

local function api(method, path, query, body_tbl)
  require_key()
  local url = build_url(path, query)
  local headers = {
    Authorization = "Bearer " .. opts.api_key,
    Accept = "application/json",
  }
  local body
  if body_tbl ~= nil then
    body = json.encode(body_tbl, false)
    headers["Content-Type"] = "application/json"
  end
  local raw, status, err = HTTP.request(method, url, headers, body)
  if err then
    -- transport-level failure (timeout, DNS, etc.) — no HTTP status
    return nil, 0, raw, err
  end
  local data
  if raw and raw ~= "" then
    local ok, decoded = pcall(json.decode, raw)
    if ok then
      data = decoded
    else
      data = raw
    end
  end
  return data, status, raw, nil
end

local function print_result(data, status, raw, err)
  if err then
    io.stderr:write("kanbn: " .. err .. "\n")
    os.exit(2)
  end
  if status < 200 or status >= 300 then
    io.stderr:write(string.format("HTTP %d\n", status))
    if type(data) == "table" then
      io.stderr:write(json.encode(data, true) .. "\n")
    elseif raw and raw ~= "" then
      io.stderr:write(raw .. "\n")
    end
    os.exit(1)
  end
  if opts.raw then
    io.write((raw or "") .. "\n")
  elseif type(data) == "table" then
    io.write(json.encode(data, true) .. "\n")
  else
    io.write(tostring(data or "") .. "\n")
  end
end

-- Require 2xx; die with body on failure.
local function api_ok(method, path, query, body_tbl)
  local data, status, raw, err = api(method, path, query, body_tbl)
  if err then
    die(string.format("%s %s failed: %s", method, path, err), 2)
  end
  if status < 200 or status >= 300 then
    local detail = raw or ""
    if type(data) == "table" then
      detail = json.encode(data, true)
    end
    die(string.format("%s %s failed (HTTP %d)\n%s", method, path, status, detail))
  end
  return data, status, raw
end

--------------------------------------------------------------------
-- filesystem helpers
--------------------------------------------------------------------

local function path_join(a, b)
  local last = a:sub(-1)
  if last == "/" or last == "\\" then return a .. b end
  return a .. "/" .. b
end

-- Stored/backup paths use "/" (portable, accepted by io.open and curl on
-- Windows). cmd.exe builtins (mkdir, if exist) need native separators.
local function native_path(p)
  if IS_WINDOWS then return (tostring(p):gsub("/", "\\")) end
  return p
end

local function mkdir_p(path)
  local ok
  if IS_WINDOWS then
    local win = native_path(path)
    -- `mkdir` in cmd.exe creates intermediate dirs; it errors if the dir
    -- already exists, so treat an existing directory as success.
    ok = os.execute("if not exist " .. shell_quote(win)
      .. " mkdir " .. shell_quote(win))
  else
    ok = os.execute("mkdir -p " .. shell_quote(path))
  end
  if not (ok == true or ok == 0) then
    die("failed to create directory: " .. path)
  end
end

local function is_dir(path)
  local ok
  if IS_WINDOWS then
    ok = os.execute("if exist " .. shell_quote(native_path(path) .. "\\*") .. " (exit 0) else (exit 1)")
  else
    ok = os.execute("test -d " .. shell_quote(path))
  end
  return ok == true or ok == 0
end

local function is_file(path)
  local ok
  if IS_WINDOWS then
    local win = native_path(path)
    -- `if exist path\` (trailing sep) is true only for directories, so a
    -- plain-existing path that is not a directory must be a file.
    ok = os.execute("if exist " .. shell_quote(win)
      .. " if not exist " .. shell_quote(win .. "\\") .. " (exit 0) else (exit 1)")
  else
    ok = os.execute("test -f " .. shell_quote(path))
  end
  return ok == true or ok == 0
end

local function file_size(path)
  local f, err = io.open(path, "rb")
  if not f then return nil, err end
  local size = f:seek("end")
  f:close()
  return size
end

local function safe_filename(name)
  name = tostring(name or "file")
  name = name:gsub("[/\\%z\n\r\t:%*?\"<>|]", "_")
  name = name:gsub("^%s+", ""):gsub("%s+$", "")
  if name == "" or name == "." or name == ".." then name = "file" end
  if #name > 180 then name = name:sub(1, 180) end
  return name
end

local function iso_now()
  return os.date("!%Y-%m-%dT%H:%M:%SZ")
end

local function stamp_now()
  return os.date("!%Y%m%dT%H%M%SZ")
end

--------------------------------------------------------------------
-- board backup / restore
--------------------------------------------------------------------

local function extract_comments(activities)
  local comments = {}
  for _, act in ipairs(activities or {}) do
    if act.type == "card.updated.comment.added" and act.comment and act.comment.comment then
      comments[#comments + 1] = {
        text = act.comment.comment,
        createdAt = act.createdAt,
        authorName = act.user and act.user.name or nil,
        authorEmail = act.user and act.user.email or nil,
        sourcePublicId = act.comment.publicId,
      }
    end
  end
  return comments
end

local function compact_checklist(checklists)
  local out = {}
  for _, cl in ipairs(checklists or {}) do
    local items = {}
    for _, it in ipairs(cl.items or {}) do
      items[#items + 1] = {
        title = it.title,
        completed = it.completed and true or false,
        index = it.index,
        sourcePublicId = it.publicId,
      }
    end
    table.sort(items, function(a, b) return (a.index or 0) < (b.index or 0) end)
    out[#out + 1] = {
      name = cl.name,
      index = cl.index,
      sourcePublicId = cl.publicId,
      items = items,
    }
  end
  table.sort(out, function(a, b) return (a.index or 0) < (b.index or 0) end)
  return out
end

local function compact_attachments(attachments)
  local out = {}
  for _, a in ipairs(attachments or {}) do
    out[#out + 1] = {
      sourcePublicId = a.publicId,
      originalFilename = a.originalFilename or a.filename,
      filename = a.filename or a.originalFilename,
      contentType = a.contentType,
      size = a.size,
      url = a.url,
      s3Key = a.s3Key,
      file = nil, -- filled if downloaded
    }
  end
  return out
end

local function backup_board(board_id, out_dir)
  log("fetching board " .. board_id .. " ...")
  local board = api_ok("GET", "/boards/" .. board_id)

  local lists = board.lists or {}
  table.sort(lists, function(a, b) return (a.index or 0) < (b.index or 0) end)

  local card_ids = {}
  for _, list in ipairs(lists) do
    for _, c in ipairs(list.cards or {}) do
      card_ids[#card_ids + 1] = c.publicId
    end
  end

  log(string.format("fetching %d card detail(s) ...", #card_ids))
  local details = {}
  for i, cid in ipairs(card_ids) do
    if i == 1 or i == #card_ids or i % 10 == 0 then
      log(string.format("  card %d/%d", i, #card_ids))
    end
    details[cid] = api_ok("GET", "/cards/" .. cid)
  end

  if not out_dir or out_dir == "" then
    local slug = board.slug or board.publicId or "board"
    out_dir = string.format("kanbn-backup-%s-%s", safe_filename(slug), stamp_now())
  end
  mkdir_p(out_dir)
  local att_root = path_join(out_dir, "attachments")

  local labels_out = {}
  for _, l in ipairs(board.labels or {}) do
    labels_out[#labels_out + 1] = {
      name = l.name,
      colourCode = l.colourCode,
      sourcePublicId = l.publicId,
    }
  end

  local lists_out = {}
  local att_count, att_ok, att_fail = 0, 0, 0
  local comment_count = 0

  for _, list in ipairs(lists) do
    local cards_out = {}
    local cards = list.cards or {}
    table.sort(cards, function(a, b) return (a.index or 0) < (b.index or 0) end)

    for _, c in ipairs(cards) do
      local d = details[c.publicId] or c
      local label_names = {}
      for _, l in ipairs(d.labels or c.labels or {}) do
        label_names[#label_names + 1] = l.name
      end

      local members = {}
      for _, m in ipairs(d.members or c.members or {}) do
        members[#members + 1] = {
          email = m.email,
          name = m.user and m.user.name or nil,
          sourcePublicId = m.publicId,
        }
      end

      local comments = extract_comments(d.activities)
      comment_count = comment_count + #comments

      local attachments = compact_attachments(d.attachments)
      if #attachments == 0 and c.attachments then
        -- board payload only has publicIds; keep stubs if detail lacked them
        for _, a in ipairs(c.attachments) do
          if a.publicId then
            attachments[#attachments + 1] = {
              sourcePublicId = a.publicId,
              originalFilename = nil,
              contentType = nil,
              size = nil,
              url = nil,
              file = nil,
            }
          end
        end
      end

      if not opts.no_attachments then
        for _, a in ipairs(attachments) do
          att_count = att_count + 1
          if a.url and a.url ~= "" then
            local fname = safe_filename(a.originalFilename or a.filename or a.sourcePublicId or "attachment")
            local rel = path_join(path_join("attachments", c.publicId), fname)
            local abs = path_join(out_dir, rel)
            mkdir_p(path_join(att_root, c.publicId))
            log("  downloading attachment: " .. rel)
            -- attachments can be large; allow 5× default budget (min 120s)
            local dl_timeout = HTTP.timeout
            if dl_timeout and dl_timeout > 0 then
              dl_timeout = math.max(120, dl_timeout * 5)
            end
            local ok, err = HTTP.download(a.url, abs, dl_timeout)
            if ok then
              a.file = rel
              a.size = a.size or file_size(abs)
              att_ok = att_ok + 1
            else
              log("  warning: failed to download " .. tostring(a.url) .. " (" .. tostring(err) .. ")")
              att_fail = att_fail + 1
            end
          else
            log("  warning: attachment " .. tostring(a.sourcePublicId) .. " has no download URL")
            att_fail = att_fail + 1
          end
        end
      end

      local card_out = {
        sourcePublicId = c.publicId,
        title = d.title or c.title,
        description = d.description or c.description or "",
        index = c.index,
        cardNumber = d.cardNumber or c.cardNumber,
        dueDate = d.dueDate or c.dueDate,
        labels = label_names,
        members = members,
        checklists = compact_checklist(d.checklists or c.checklists),
        comments = comments,
        attachments = attachments,
      }
      if not opts.skip_activities then
        card_out.activities = d.activities
      end
      cards_out[#cards_out + 1] = card_out
    end

    lists_out[#lists_out + 1] = {
      name = list.name,
      index = list.index,
      sourcePublicId = list.publicId,
      cards = cards_out,
    }
  end

  local backup = {
    format = BACKUP_FORMAT,
    version = BACKUP_VERSION,
    exportedAt = iso_now(),
    tool = "kanbn.lua",
    toolVersion = VERSION,
    source = {
      baseUrl = opts.base,
      boardPublicId = board.publicId,
      boardName = board.name,
      boardSlug = board.slug,
      workspacePublicId = board.workspace and board.workspace.publicId or nil,
      cardPrefix = board.workspace and board.workspace.cardPrefix or nil,
    },
    board = {
      name = board.name,
      slug = board.slug,
      visibility = board.visibility,
      favorite = board.favorite,
      isArchived = board.isArchived,
      labels = labels_out,
      lists = lists_out,
    },
    stats = {
      lists = #lists_out,
      cards = #card_ids,
      labels = #labels_out,
      comments = comment_count,
      attachments = att_count,
      attachmentsDownloaded = att_ok,
      attachmentsFailed = att_fail,
    },
  }

  local board_json_path = path_join(out_dir, "board.json")
  write_file(board_json_path, json.encode(backup, true) .. "\n")

  log(string.format(
    "backup written to %s (%d lists, %d cards, %d labels, %d comments, %d attachments downloaded)",
    out_dir, #lists_out, #card_ids, #labels_out, comment_count, att_ok
  ))
  return out_dir, backup
end

local function load_backup(path)
  local json_path = path
  if is_dir(path) then
    json_path = path_join(path, "board.json")
  end
  if not is_file(json_path) then
    die("backup not found: " .. json_path)
  end
  local raw, err = read_file(json_path)
  if not raw then die("cannot read " .. json_path .. ": " .. tostring(err)) end
  local ok, data = pcall(json.decode, raw)
  if not ok then die("invalid JSON in " .. json_path .. ": " .. tostring(data)) end
  if type(data) ~= "table" or data.format ~= BACKUP_FORMAT then
    die("not a " .. BACKUP_FORMAT .. " file: " .. json_path)
  end
  if (data.version or 0) > BACKUP_VERSION then
    die(string.format(
      "backup version %s is newer than this tool (supports %d)",
      tostring(data.version), BACKUP_VERSION
    ))
  end
  -- strip the trailing filename, tolerating both / and \ separators
  local root = is_dir(path) and path or (json_path:match("^(.*)[/\\][^/\\]+$") or ".")
  return data, root, json_path
end

local function restore_board(path)
  local backup, root = load_backup(path)
  local b = backup.board
  if not b then die("backup missing board payload") end

  local workspace = opts.workspace
    or (backup.source and backup.source.workspacePublicId)
  if not workspace or workspace == "" then
    die("restore needs --workspace <publicId> (backup has no source workspace)")
  end

  local board_name = opts.name or b.name or "Restored board"
  local lists = b.lists or {}
  table.sort(lists, function(a, c) return (a.index or 0) < (c.index or 0) end)

  local list_names = {}
  local total_cards = 0
  for _, list in ipairs(lists) do
    list_names[#list_names + 1] = list.name or ("List " .. tostring(#list_names + 1))
    total_cards = total_cards + #(list.cards or {})
  end

  local labels = b.labels or {}
  local label_names_for_create = {}
  for _, l in ipairs(labels) do
    if l.name then label_names_for_create[#label_names_for_create + 1] = l.name end
  end

  log(string.format(
    "restore plan: workspace=%s name=%q lists=%d cards=%d labels=%d",
    workspace, board_name, #lists, total_cards, #labels
  ))

  if opts.dry_run then
    for _, list in ipairs(lists) do
      log(string.format("  list %q — %d cards", list.name or "?", #(list.cards or {})))
      for _, c in ipairs(list.cards or {}) do
        local nlab = #(c.labels or {})
        local nchk = #(c.checklists or {})
        local ncom = #(c.comments or {})
        local natt = #(c.attachments or {})
        log(string.format(
          "    - %s  labels=%d checklists=%d comments=%d attachments=%d",
          c.title or "(untitled)", nlab, nchk, ncom, natt
        ))
      end
    end
    log("dry-run complete (no changes made)")
    return nil
  end

  -- Create board with list names; labels created separately so colours apply.
  log("creating board ...")
  local created = api_ok("POST", "/workspaces/" .. workspace .. "/boards", nil, {
    name = board_name,
    lists = list_names,
    labels = {},
    type = "regular",
  })
  local new_board_id = created.publicId
  if not new_board_id then die("create board returned no publicId") end
  log("created board " .. new_board_id)

  -- Map lists by name (first-match for duplicate names).
  local live = api_ok("GET", "/boards/" .. new_board_id)
  local list_id_by_name = {}
  local list_ids_ordered = {}
  local live_lists = live.lists or {}
  table.sort(live_lists, function(a, c) return (a.index or 0) < (c.index or 0) end)
  for i, list in ipairs(lists) do
    local live_list = live_lists[i]
    local id = live_list and live_list.publicId
    if id then
      list_id_by_name[list.name] = list_id_by_name[list.name] or id
      list_ids_ordered[i] = id
      -- ensure name/index match
      if live_list.name ~= list.name or live_list.index ~= list.index then
        api_ok("PUT", "/lists/" .. id, nil, {
          name = list.name,
          index = list.index,
        })
      end
    else
      -- create missing list
      local nl = api_ok("POST", "/lists", nil, {
        name = list.name,
        boardPublicId = new_board_id,
      })
      list_id_by_name[list.name] = list_id_by_name[list.name] or nl.publicId
      list_ids_ordered[i] = nl.publicId
      if list.index then
        api_ok("PUT", "/lists/" .. nl.publicId, nil, {
          name = list.name,
          index = list.index,
        })
      end
    end
  end

  -- Labels with colours
  local label_id_by_name = {}
  for _, l in ipairs(labels) do
    local colour = l.colourCode or "#6366f1"
    if type(colour) ~= "string" or #colour ~= 7 then colour = "#6366f1" end
    local created_label = api_ok("POST", "/labels", nil, {
      name = l.name,
      boardPublicId = new_board_id,
      colourCode = colour,
    })
    if created_label.publicId then
      label_id_by_name[l.name] = created_label.publicId
    end
  end

  -- Board meta
  local board_update = {}
  if b.visibility then board_update.visibility = b.visibility end
  if b.favorite ~= nil then board_update.favorite = b.favorite end
  if b.isArchived ~= nil then board_update.isArchived = b.isArchived end
  -- slug is often unique; only set if present and different enough — skip by default
  if next(board_update) then
    api_ok("PUT", "/boards/" .. new_board_id, nil, board_update)
  end

  local cards_done = 0
  for li, list in ipairs(lists) do
    local list_id = list_ids_ordered[li] or list_id_by_name[list.name]
    if not list_id then
      die("could not resolve list id for " .. tostring(list.name))
    end
    local cards = list.cards or {}
    table.sort(cards, function(a, c) return (a.index or 0) < (c.index or 0) end)

    for _, card in ipairs(cards) do
      cards_done = cards_done + 1
      log(string.format("  card %d/%d: %s", cards_done, total_cards, card.title or "?"))

      local label_ids = {}
      for _, lname in ipairs(card.labels or {}) do
        local lid = label_id_by_name[lname]
        if lid then label_ids[#label_ids + 1] = lid end
      end

      local created_card = api_ok("POST", "/cards", nil, {
        title = card.title or "Untitled",
        description = card.description or "",
        listPublicId = list_id,
        labelPublicIds = label_ids,
        memberPublicIds = {},
        position = "end",
        dueDate = card.dueDate,
      })
      local new_card_id = created_card.publicId
      if not new_card_id then die("create card returned no publicId") end

      if card.index ~= nil then
        api_ok("PUT", "/cards/" .. new_card_id, nil, {
          index = card.index,
          listPublicId = list_id,
        })
      end

      -- Checklists
      local checklists = card.checklists or {}
      table.sort(checklists, function(a, c) return (a.index or 0) < (c.index or 0) end)
      for _, cl in ipairs(checklists) do
        local created_cl = api_ok("POST", "/cards/" .. new_card_id .. "/checklists", nil, {
          name = cl.name or "Checklist",
        })
        local cl_id = created_cl.publicId
        local items = cl.items or {}
        table.sort(items, function(a, c) return (a.index or 0) < (c.index or 0) end)
        for _, it in ipairs(items) do
          local created_it = api_ok("POST", "/checklists/" .. cl_id .. "/items", nil, {
            title = it.title or "Item",
          })
          if it.completed and created_it.publicId then
            api_ok("PATCH", "/checklists/items/" .. created_it.publicId, nil, {
              completed = true,
            })
          end
        end
      end

      -- Comments (chronological)
      if not opts.skip_comments then
        for _, com in ipairs(card.comments or {}) do
          local text = com.text or com.comment
          if text and text ~= "" then
            api_ok("POST", "/cards/" .. new_card_id .. "/comments", nil, {
              comment = text,
            })
          end
        end
      end

      -- Attachments
      if not opts.skip_attachments then
        for _, att in ipairs(card.attachments or {}) do
          local rel = att.file
          if rel and rel ~= "" then
            local abs = path_join(root, rel)
            if not is_file(abs) then
              log("  warning: missing attachment file " .. abs)
            else
              local fname = att.originalFilename or att.filename or safe_filename(rel:match("([^/]+)$") or "file")
              local ctype = att.contentType or "application/octet-stream"
              local size = att.size or file_size(abs)
              if not size or size <= 0 then
                log("  warning: skip empty attachment " .. abs)
              else
                local up = api_ok("POST", "/cards/" .. new_card_id .. "/attachments/upload-url", nil, {
                  filename = fname,
                  contentType = ctype,
                  size = size,
                })
                if not up.url or not up.key then
                  log("  warning: no upload URL for " .. fname)
                else
                  -- attachments can be large; allow 5× default budget (min 120s)
                  local up_timeout = HTTP.timeout
                  if up_timeout and up_timeout > 0 then
                    up_timeout = math.max(120, up_timeout * 5)
                  end
                  local st, up_err = HTTP.upload_file("PUT", up.url, {
                    ["Content-Type"] = ctype,
                  }, abs, up_timeout)
                  if up_err then
                    log("  warning: upload failed for " .. fname .. ": " .. up_err)
                  elseif st < 200 or st >= 300 then
                    log("  warning: upload HTTP " .. tostring(st) .. " for " .. fname)
                  else
                    api_ok("POST", "/cards/" .. new_card_id .. "/attachments/confirm", nil, {
                      s3Key = up.key,
                      filename = fname,
                      originalFilename = fname,
                      contentType = ctype,
                      size = size,
                    })
                  end
                end
              end
            end
          end
        end
      end
    end
  end

  log(string.format("restore complete: board %s (%q)", new_board_id, board_name))
  local summary = {
    boardPublicId = new_board_id,
    name = board_name,
    workspacePublicId = workspace,
    lists = #lists,
    cards = total_cards,
    labels = #labels,
  }
  io.write(json.encode(summary, true) .. "\n")
  return new_board_id, summary
end

--------------------------------------------------------------------
-- pretty board exploration (no extra requests beyond board GET)
--------------------------------------------------------------------

local function label_names(labels)
  if not labels or #labels == 0 then return "" end
  local names = {}
  for _, l in ipairs(labels) do
    names[#names + 1] = l.name or "?"
  end
  return table.concat(names, ", ")
end

local function explore_board(board)
  local lines = {}
  local function w(s) lines[#lines + 1] = s end

  w(string.format("Board: %s  (%s)", board.name or "?", board.publicId or "?"))
  if board.slug then w("  slug:       " .. board.slug) end
  if board.visibility then w("  visibility: " .. board.visibility) end
  w(string.format("  favorite:   %s   archived: %s",
    tostring(board.favorite), tostring(board.isArchived)))
  if board.workspace then
    w(string.format("  workspace:  %s  prefix=%s",
      board.workspace.publicId or "?",
      board.workspace.cardPrefix or "?"))
  end

  if board.labels and #board.labels > 0 then
    w("")
    w("Labels:")
    for _, l in ipairs(board.labels) do
      w(string.format("  - %s  %s  (%s)",
        l.name or "?", l.colourCode or "-", l.publicId or "?"))
    end
  end

  local lists = board.lists or {}
  table.sort(lists, function(a, b)
    return (a.index or 0) < (b.index or 0)
  end)

  local total = 0
  w("")
  w(string.format("Lists (%d):", #lists))
  for _, list in ipairs(lists) do
    local cards = list.cards or {}
    table.sort(cards, function(a, b)
      return (a.index or 0) < (b.index or 0)
    end)
    total = total + #cards
    w("")
    w(string.format("## [%d] %s  (%s)  — %d cards",
      list.index or 0, list.name or "?", list.publicId or "?", #cards))
    if #cards == 0 then
      w("   (empty)")
    else
      for _, c in ipairs(cards) do
        local prefix = (board.workspace and board.workspace.cardPrefix) or ""
        local num = c.cardNumber and (prefix .. "-" .. tostring(c.cardNumber)) or "?"
        local labs = label_names(c.labels)
        local due = c.dueDate and ("  due:" .. c.dueDate) or ""
        local att = (c.attachments and #c.attachments > 0)
          and string.format("  [%d att]", #c.attachments) or ""
        local chk = ""
        if c.checklists and #c.checklists > 0 then
          local done, all = 0, 0
          for _, cl in ipairs(c.checklists) do
            for _, it in ipairs(cl.items or {}) do
              all = all + 1
              if it.completed then done = done + 1 end
            end
          end
          chk = string.format("  chk:%d/%d", done, all)
        end
        local members = ""
        if c.members and #c.members > 0 then
          local mnames = {}
          for _, m in ipairs(c.members) do
            local name = (m.user and m.user.name) or m.email or "?"
            mnames[#mnames + 1] = name
          end
          members = "  @" .. table.concat(mnames, ",")
        end
        w(string.format("  - %s  %s  (%s)%s%s%s%s%s",
          num,
          c.title or "(no title)",
          c.publicId or "?",
          labs ~= "" and ("  [" .. labs .. "]") or "",
          due, att, chk, members))
        if c.description and c.description ~= "" then
          -- strip simple HTML tags from Kan card descriptions
          local desc = c.description
            :gsub("<br%s*/?>", " ")
            :gsub("</p>", " ")
            :gsub("<[^>]+>", "")
            :gsub("&nbsp;", " ")
            :gsub("&amp;", "&")
            :gsub("&lt;", "<")
            :gsub("&gt;", ">")
            :gsub("&quot;", '"')
            :gsub("&#(%d+);", function(n)
              n = tonumber(n)
              return (n and n >= 32 and n < 127) and string.char(n) or ""
            end)
            :gsub("%s+", " ")
            :gsub("^%s+", "")
            :gsub("%s+$", "")
          if #desc > 120 then desc = desc:sub(1, 117) .. "..." end
          if desc ~= "" then w("      " .. desc) end
        end
      end
    end
  end

  w("")
  w(string.format("Total cards: %d", total))
  return table.concat(lines, "\n")
end

--------------------------------------------------------------------
-- CLI parsing
--------------------------------------------------------------------

local function usage()
  io.write([[kanbn.lua v]] .. VERSION .. [[ — Kan REST API CLI

Env:
  KANBN_API_KEY            required Bearer token (environment or .env)
  KANBN_TIMEOUT            default total timeout seconds (default ]] .. tostring(TIMEOUT_DEFAULT) .. [[)
  KANBN_CONNECT_TIMEOUT    default connect timeout seconds (default ]] .. tostring(CONNECT_TIMEOUT_DEFAULT) .. [[)

Commands:
  me
  workspaces
  workspace <id-or-slug>
  boards <workspacePublicId>
  board <boardPublicId>
  card <cardPublicId | GEN-N>
  card update <cardPublicId | GEN-N> <key=value ...>   # e.g. description=<html>
  card move <cardPublicId | GEN-N> <list name>
  card-by-number <workspacePublicId> <GEN-N | N>   # -> publicId
  search <workspacePublicId> <query> [--limit N]
  find-workspace <name>
  find-board <workspacePublicId> <name>
  explore-board <boardPublicId>
  backup-board <boardPublicId> [out-dir]
  restore-board <backup-path>
  checklist add <cardPublicId | GEN-N> [--ws WS] <name>   # -> checklist publicId
  checklist-item add <checklistPublicId> <title>          # -> item publicId
  get <path> [key=value ...]
  request <METHOD> <path> [json-body | --body-file PATH]

Flags:
  --raw                 raw JSON body
  --base URL            API base (default ]] .. BASE_DEFAULT .. [[)
  --quiet               less stderr
  --timeout SEC         total HTTP timeout (default ]] .. tostring(TIMEOUT_DEFAULT) .. [[; 0 = none)
  --connect-timeout SEC connect timeout (default ]] .. tostring(CONNECT_TIMEOUT_DEFAULT) .. [[; 0 = none)
  --no-attachments      backup: metadata only (no file download)
  --skip-activities     backup: omit activity history
  --workspace ID        restore: target workspace public id
  --name NAME           restore: board name override
  --body-file PATH      request: read JSON body from file (Windows-safe)
  --dry-run             restore: plan only
  --skip-attachments    restore: do not upload files
  --skip-comments       restore: do not recreate comments
  -h, --help            this help

Backup format:
  Creates a directory with board.json plus optional attachments/.
  Restore rebuilds lists, labels, cards, checklists, comments, and
  files into a NEW board (IDs/card numbers are not preserved).

Examples:
  kanbn.lua workspaces
  kanbn.lua explore-board mx87hw9x3zf3
  kanbn.lua backup-board mx87hw9x3zf3
  kanbn.lua backup-board mx87hw9x3zf3 ./wire-backup
  kanbn.lua restore-board ./wire-backup --workspace 0w1w9dpim929 --dry-run
  kanbn.lua restore-board ./wire-backup --workspace 0w1w9dpim929 --name "Wire restored"
]])
end

local args = { ... }
local i = 1
local positional = {}

local VALUE_FLAGS = {
  ["--base"] = function(v) opts.base = v end,
  ["--workspace"] = function(v) opts.workspace = v end,
  ["--name"] = function(v) opts.name = v end,
  ["--body-file"] = function(v) opts.body_file = v end,
  ["--limit"] = function(v) opts.limit = tonumber(v) end,
  ["--timeout"] = function(v)
    local n = parse_timeout_seconds(v, "--timeout")
    opts.timeout = n
    HTTP.timeout = n
  end,
  ["--connect-timeout"] = function(v)
    local n = parse_timeout_seconds(v, "--connect-timeout")
    opts.connect_timeout = n
    HTTP.connect_timeout = n
  end,
}

local BOOL_FLAGS = {
  ["--raw"] = function() opts.raw = true end,
  ["--quiet"] = function() opts.quiet = true end,
  ["--dry-run"] = function() opts.dry_run = true end,
  ["--no-attachments"] = function() opts.no_attachments = true end,
  ["--skip-attachments"] = function() opts.skip_attachments = true end,
  ["--skip-comments"] = function() opts.skip_comments = true end,
  ["--skip-activities"] = function() opts.skip_activities = true end,
}

while i <= #args do
  local a = args[i]
  if a == "-h" or a == "--help" then
    usage()
    os.exit(0)
  elseif VALUE_FLAGS[a] then
    i = i + 1
    local v = args[i] or die(a .. " needs a value")
    VALUE_FLAGS[a](v)
  elseif BOOL_FLAGS[a] then
    BOOL_FLAGS[a]()
  elseif a:sub(1, 1) == "-" then
    die("unknown flag: " .. a)
  else
    positional[#positional + 1] = a
  end
  i = i + 1
end

local cmd = positional[1]
if not cmd then
  usage()
  os.exit(1)
end

local function need(n, msg)
  if not positional[n] then die(msg) end
  return positional[n]
end

--------------------------------------------------------------------
-- card number resolution: "GEN-19" / "19" -> card publicId
--------------------------------------------------------------------

-- Resolve a card "number" to its publicId within a workspace. Accepts either
-- a bare number ("19") or a prefixed string ("GEN-19"); the prefix is ignored
-- (card numbers are unique per workspace). Returns publicId, or dies.
local function resolve_card_in_workspace(ws_id, card_ref)
  local num = tostring(card_ref):match("^(%d+)$")
    or tostring(card_ref):match("%-(%d+)$")
  if not num then
    die("card reference must look like '19' or 'GEN-19', got: " .. tostring(card_ref))
  end
  -- Search the workspace; the API matches card number via the query.
  local data, status, raw, err = api("GET", "/workspaces/" .. ws_id .. "/search", {
    query = num,
    limit = opts.limit or 50,
  })
  if err or status < 200 or status >= 300 then
    print_result(data, status, raw, err)
  end
  -- Search may return cards and/or boards; pull the matching card number.
  local results = data.cards or data or {}
  for _, c in ipairs(results) do
    local cnum = c.cardNumber and tostring(c.cardNumber) or nil
    if cnum == num and c.publicId then
      return c.publicId, tonumber(num)
    end
  end
  -- Fallback: scan every board in the workspace for the exact card number.
  local boards = api_ok("GET", "/workspaces/" .. ws_id .. "/boards")
  for _, b in ipairs(boards or {}) do
    local board = api_ok("GET", "/boards/" .. b.publicId)
    for _, list in ipairs(board.lists or {}) do
      for _, c in ipairs(list.cards or {}) do
        local cnum = c.cardNumber and tostring(c.cardNumber) or nil
        if cnum == num and c.publicId then
          return c.publicId, tonumber(num)
        end
      end
    end
  end
  die("no card with number " .. num .. " found in workspace " .. ws_id)
end

local DEFAULT_WORKSPACE = "0w1w9dpim929"

local function is_public_id(value)
  return tostring(value):match("^%w+$") ~= nil and #tostring(value) >= 12
end

local function resolve_card_reference(card_ref)
  if is_public_id(card_ref) then return card_ref end
  return resolve_card_in_workspace(opts.workspace or DEFAULT_WORKSPACE, card_ref)
end

local function resolve_list_in_card(card, list_ref)
  local lists = card.list and card.list.board and card.list.board.lists or {}
  local wanted = trim(list_ref):lower()
  for _, list in ipairs(lists) do
    if list.publicId == list_ref or trim(list.name):lower() == wanted then
      return list.publicId
    end
  end
  local available = {}
  for _, list in ipairs(lists) do
    available[#available + 1] = list.name
  end
  die("unknown list '" .. tostring(list_ref) .. "'; available lists: " .. table.concat(available, ", "))
end

--------------------------------------------------------------------
-- commands
--------------------------------------------------------------------

if cmd == "me" then
  local data, status, raw, err = api("GET", "/users/me")
  print_result(data, status, raw, err)

elseif cmd == "workspaces" then
  local data, status, raw, err = api("GET", "/workspaces")
  print_result(data, status, raw, err)

elseif cmd == "workspace" then
  local id = need(2, "usage: workspace <id-or-slug>")
  local data, status, raw, err = api("GET", "/workspaces/" .. id)
  print_result(data, status, raw, err)

elseif cmd == "boards" then
  local ws = need(2, "usage: boards <workspacePublicId>")
  local data, status, raw, err = api("GET", "/workspaces/" .. ws .. "/boards")
  print_result(data, status, raw, err)

elseif cmd == "board" then
  local id = need(2, "usage: board <boardPublicId>")
  local data, status, raw, err = api("GET", "/boards/" .. id)
  print_result(data, status, raw, err)

elseif cmd == "card" and positional[2] == "move" then
  local ref = need(3, "usage: card move <cardPublicId | GEN-N> <list name>")
  local list_parts = {}
  for ai = 4, #positional do
    list_parts[#list_parts + 1] = positional[ai]
  end
  if #list_parts == 0 then
    die("usage: card move <cardPublicId | GEN-N> <list name>")
  end
  local list_ref = table.concat(list_parts, " ")
  local card_id = resolve_card_reference(ref)
  local card = api_ok("GET", "/cards/" .. card_id)
  local list_id = resolve_list_in_card(card, list_ref)
  local data, status, raw, err = api("PUT", "/cards/" .. card_id, nil, {
    listPublicId = list_id,
  })
  print_result(data, status, raw, err)

elseif cmd == "card" and positional[2] == "update" then
  -- card update <cardPublicId | GEN-N> <key=value ...>   (e.g. description=...)
  local ref = need(3, "usage: card update <cardPublicId | GEN-N> <key=value ...>")
  local id = resolve_card_reference(ref)
  local body = {}
  for ai = 4, #positional do
    local k, v = positional[ai]:match("^([^=]+)=(.*)$")
    if not k then die("args must be key=value, got: " .. positional[ai]) end
    body[k] = v
  end
  if not next(body) then die("card update needs at least one key=value field") end
  local data, status, raw, err = api("PUT", "/cards/" .. id, nil, body)
  print_result(data, status, raw, err)

elseif cmd == "card" then
  local id = resolve_card_reference(need(2, "usage: card <cardPublicId | GEN-N>"))
  local data, status, raw, err = api("GET", "/cards/" .. id)
  print_result(data, status, raw, err)

elseif cmd == "search" then
  local ws = need(2, "usage: search <workspacePublicId> <query> [--limit N]")
  local q = need(3, "usage: search <workspacePublicId> <query>")
  local limit
  for ai = 4, #positional do
    if positional[ai] == "--limit" then
      limit = tonumber(positional[ai + 1])
    elseif positional[ai]:match("^%d+$") and not limit then
      limit = tonumber(positional[ai])
    end
  end
  local data, status, raw, err = api("GET", "/workspaces/" .. ws .. "/search", {
    query = q,
    limit = limit or opts.limit or 20,
  })
  print_result(data, status, raw, err)

elseif cmd == "find-workspace" then
  local name = need(2, "usage: find-workspace <name>")
  local data, status, raw, err = api("GET", "/workspaces")
  if err or status < 200 or status >= 300 then
    print_result(data, status, raw, err)
  end
  local needle = name:lower()
  local hits = {}
  for _, row in ipairs(data or {}) do
    local ws = row.workspace or row
    local n = (ws.name or ""):lower()
    if n == needle or n:find(needle, 1, true) then
      hits[#hits + 1] = row
    end
  end
  if #hits == 0 then die("no workspace matching '" .. name .. "'", 2) end
  if opts.raw then
    io.write(json.encode(hits, false) .. "\n")
  else
    io.write(json.encode(#hits == 1 and hits[1] or hits, true) .. "\n")
  end

elseif cmd == "find-board" then
  local ws = need(2, "usage: find-board <workspacePublicId> <name>")
  local name = need(3, "usage: find-board <workspacePublicId> <name>")
  local data, status, raw, err = api("GET", "/workspaces/" .. ws .. "/boards")
  if err or status < 200 or status >= 300 then
    print_result(data, status, raw, err)
  end
  local needle = name:lower()
  local hits = {}
  for _, b in ipairs(data or {}) do
    local n = (b.name or ""):lower()
    if n == needle or n:find(needle, 1, true) then
      hits[#hits + 1] = b
    end
  end
  if #hits == 0 then die("no board matching '" .. name .. "'", 2) end
  if opts.raw then
    io.write(json.encode(hits, false) .. "\n")
  else
    io.write(json.encode(#hits == 1 and hits[1] or hits, true) .. "\n")
  end

elseif cmd == "explore-board" then
  local id = need(2, "usage: explore-board <boardPublicId>")
  local data, status, raw, err = api("GET", "/boards/" .. id)
  if err or status < 200 or status >= 300 then
    print_result(data, status, raw, err)
  end
  if opts.raw then
    io.write(raw .. "\n")
  else
    io.write(explore_board(data) .. "\n")
  end

elseif cmd == "backup-board" then
  local id = need(2, "usage: backup-board <boardPublicId> [out-dir]")
  local out = positional[3]
  local dir = backup_board(id, out)
  if opts.raw then
    io.write(dir .. "\n")
  else
    io.write(string.format("Backup directory: %s\n", dir))
    io.write(string.format("Manifest: %s/board.json\n", dir))
  end

elseif cmd == "restore-board" then
  local path = need(2, "usage: restore-board <backup-path> [--workspace ID] [--name NAME] [--dry-run]")
  restore_board(path)

elseif cmd == "get" then
  local path = need(2, "usage: get <path> [key=value ...]")
  local query = {}
  for ai = 3, #positional do
    local k, v = positional[ai]:match("^([^=]+)=(.*)$")
    if k then query[k] = v else die("query args must be key=value, got: " .. positional[ai]) end
  end
  local data, status, raw, err = api("GET", path, query)
  print_result(data, status, raw, err)

elseif cmd == "request" then
  local method = need(2, "usage: request <METHOD> <path> [json-body | --body-file PATH]")
  local path = need(3, "usage: request <METHOD> <path> [json-body | --body-file PATH]")
  local body_tbl
  -- Prefer --body-file: passing raw JSON as a CLI arg is unreliable on
  -- Windows (cmd.exe strips embedded double quotes).
  local body_str = positional[4]
  if opts.body_file then
    local data, ferr = read_file(opts.body_file)
    if not data then die("cannot read --body-file " .. opts.body_file .. ": " .. tostring(ferr)) end
    body_str = data
  end
  if body_str then
    local ok, decoded = pcall(json.decode, body_str)
    if not ok then die("body is not valid JSON: " .. tostring(decoded)) end
    body_tbl = decoded
  end
  local data, status, raw, err = api(method:upper(), path, nil, body_tbl)
  print_result(data, status, raw, err)

elseif cmd == "card-by-number" then
  local ws = need(2, "usage: card-by-number <workspacePublicId> <GEN-N | N>")
  local ref = need(3, "usage: card-by-number <workspacePublicId> <GEN-N | N>")
  local public_id, num = resolve_card_in_workspace(ws, ref)
  if opts.raw then
    io.write(public_id .. "\n")
  else
    io.write(json.encode({ cardNumber = num, publicId = public_id }, true) .. "\n")
  end

elseif cmd == "checklist" then
  -- checklist add <cardPublicId | GEN-N> [--ws workspacePublicId] <name>
  local sub = need(2, "usage: checklist add <cardPublicId | GEN-N> [--ws WS] <name>")
  if sub ~= "add" then die("unknown checklist subcommand: " .. sub) end
  local card_arg = need(3, "usage: checklist add <cardPublicId | GEN-N> [--ws WS] <name>")
  local name = need(4, "usage: checklist add <cardPublicId | GEN-N> [--ws WS] <name>")
  -- Resolve card reference: a bare publicId is used directly; a "GEN-N" style
  -- string is resolved via the workspace (default: --ws flag, else 0w1w9dpim929).
  local card_id = resolve_card_reference(card_arg)
  local created = api_ok("POST", "/cards/" .. card_id .. "/checklists", nil, {
    name = name,
  })
  io.write(json.encode(created, true) .. "\n")

elseif cmd == "checklist-item" then
  -- checklist-item add <checklistPublicId> <title>
  local sub = need(2, "usage: checklist-item add <checklistPublicId> <title>")
  if sub ~= "add" then die("unknown checklist-item subcommand: " .. sub) end
  local cl_id = need(3, "usage: checklist-item add <checklistPublicId> <title>")
  local title = need(4, "usage: checklist-item add <checklistPublicId> <title>")
  local created = api_ok("POST", "/checklists/" .. cl_id .. "/items", nil, {
    title = title,
  })
  io.write(json.encode(created, true) .. "\n")

else
  die("unknown command: " .. cmd .. " (try --help)")
end
