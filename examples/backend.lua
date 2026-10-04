-- SPDX-License-Identifier: GPL-3.0-or-later
-- Reference bridge protocol (tab-separated fields; tabs/newlines in values
-- must be escaped by a real adapter):
-- workspaces_begin
-- workspace<TAB>id<TAB>index<TAB>monitor<TAB>focused<TAB>visible<TAB>urgent<TAB>occupied<TAB>name
-- workspaces_end
-- window<TAB>id<TAB>title<TAB>class
-- no_window
-- The bridge sends snapshots initially and after every change. Boolean flags
-- are 0/1. Client writes "snapshot\n" and "switch_workspace\t<id>\n".
local bar = require("cybar")
local M = {}

function M.connect(opts)
  assert(type(opts.path) == "string", "backend opts.path is required")
  local socket = bar.socket.connect(opts.path)
  local conn = {}
  local handlers, workspaces, pending, active = {}, {}, {}, nil
  local buffer, closed = "", false

  local function emit(event)
    local list = handlers[event] or {}
    for _, entry in ipairs(list) do
      if entry.active then
        local ok, err = pcall(entry.callback)
        if not ok then io.stderr:write("cybar backend: " .. tostring(err) .. "\n") end
      end
    end
  end

  function conn:subscribe(event, callback)
    assert(type(callback) == "function", "subscription callback must be a function")
    handlers[event] = handlers[event] or {}
    local entry = { callback = callback, active = true }
    table.insert(handlers[event], entry)
    local subscription = {}
    function subscription:remove() entry.active = false end
    subscription.cancel = subscription.remove
    return subscription
  end
  conn.on = conn.subscribe

  function conn:get_workspaces() return workspaces end
  function conn:get_active_window() return active end
  function conn:fd() return closed and -1 or socket:fd() end

  function conn:command(name, id)
    if closed then return false, "bridge socket is closed" end
    if name ~= "switch_workspace" then return false, "unsupported command: " .. tostring(name) end
    if type(id) ~= "number" or id < 0 or id % 1 ~= 0 then return false, "invalid workspace ID" end
    socket:send("switch_workspace\t" .. tostring(id) .. "\n")
    return true
  end

  local function process(line)
    if line == "workspaces_begin" then pending = {}; return end
    if line == "workspaces_end" then workspaces = pending; emit("workspace_changed"); return end
    if line == "no_window" then active = nil; emit("window_title_changed"); return end
    local fields = {}
    for field in (line .. "\t"):gmatch("(.-)\t") do table.insert(fields, field) end
    if fields[1] == "workspace" then
      local id, index = tonumber(fields[2]), tonumber(fields[3])
      if not id or not index or #fields < 9 then error("invalid workspace snapshot") end
      table.insert(pending, {
        id = id, index = index, monitor = fields[4],
        focused = fields[5] == "1", visible = fields[6] == "1",
        urgent = fields[7] == "1", occupied = fields[8] == "1", name = fields[9],
      })
    elseif fields[1] == "window" then
      local id = tonumber(fields[2])
      if not id or #fields < 4 then error("invalid active-window snapshot") end
      active = { id = id, title = fields[3], class = fields[4] }
      emit("window_title_changed")
    end
  end

  function conn:main()
    while not closed do
      local bytes = socket:read()
      if bytes == nil then closed = true; break end
      if bytes == "" then break end
      buffer = buffer .. bytes
      assert(#buffer <= 1024 * 1024, "bridge buffer exceeds 1 MiB")
      while true do
        local newline = buffer:find("\n", 1, true)
        if not newline then break end
        local line = buffer:sub(1, newline - 1):gsub("\r$", "")
        buffer = buffer:sub(newline + 1)
        process(line)
      end
    end
  end

  socket:send("snapshot\n")
  return conn
end

return M
