local env = {
    assert = assert,
    type = type,
    tonumber = tonumber,
    tostring = tostring,
    require = require,
    error = error,
    getmetatable = getmetatable,
    setmetatable = setmetatable,
    string = string,
    table = table,
    jit = jit,
}
setfenv(1, env)

local path = (...):gsub("[^%.]*$", "")
local M = require(path .. "master")
local ffi = require("ffi")

local C = M.C
local _common = M._common

-- add metamethods to ImVec2 and ImVec4

local ct = ffi.typeof("ImVec2")
local ImVec2 = {}
function ImVec2.__add(u, v)
    assert(type(u) == type(v) and ffi.istype(u, v), "One of the summands in not an ImVec2.")
    return ct(u.x + v.x, u.y + v.y)
end
function ImVec2.__sub(u, v)
    assert(type(u) == type(v) and ffi.istype(u, v), "One of the summands in not an ImVec2.")
    return ct( u.x - v.x, u.y - v.y)
end
function ImVec2.__unm(u)
    return ct(-u.x, -u.y)
end
function ImVec2.__mul(u, v)
    local nu, nv = tonumber(u), tonumber(v)
    if nu then
        return ct(nu*v.x, nu*v.y)
    elseif nv then
        return ct(nv*u.x, nv*u.y)
    else
        error("ImVec2 can only be multipliead by a numerical type.")
    end
end
function ImVec2.__div(u, a)
    a = assert(tonumber(a), "ImVec2 can only be divided by a numerical type.")
    return ct(u.x/a, u.y/a)
end

local ct = ffi.typeof("ImVec4")
local ImVec4 = {}
function ImVec4.__add(u, v)
    assert(type(u) == type(v) and ffi.istype(u, v), "One of the summands in not an ImVec4.")
    return ct(u.x + v.x, u.y + v.y, u.z + v.z, u.w + v.w)
end
function ImVec4.__sub(u, v)
    assert(type(u) == type(v) and ffi.istype(u, v), "One of the summands in not an ImVec4.")
    return ct(u.x - v.x, u.y - v.y, u.z - v.z, u.w - v.w)
end
function ImVec4.__unm(u)
    return ct(-u.x, -u.y, -u.z, -u.w)
end
function ImVec4.__mul(u, v)
    local nu, nv = tonumber(u), tonumber(v)
    if nu then
        return v:__new(nu*v.x, nu*v.y, nu*v.z, nu*v.w)
    elseif nv then
        return ct(nv*u.x, nv*u.y, nv*u.z, nv*u.w)
    else
        error("ImVec4 can only be multipliead by a numerical type.")
    end
end
function ImVec4.__div(u, a)
    a = assert(tonumber(a), "ImVec4 can only be divided by a numerical type.")
    return ct(u.x/a, u.y/a, u.z/a, u.w/a)
end

-- wrap FLT_MIN, FLT_MAX

local FLT_MIN, FLT_MAX = C.igGET_FLT_MIN(), C.igGET_FLT_MAX()
M.FLT_MIN, M.FLT_MAX = FLT_MIN, FLT_MAX

-- handwritten functions


-----------------------
-- BEGIN GENERATED CODE
-----------------------

local ImDrawList = ImDrawList or {}
ImDrawList.__index = ImDrawList
ImDrawList["AddCircleFilled"] = ImDrawList["AddCircleFilled"]  or function(i1, i2, i3, i4, i5)
    jit.off(true)
    if i5 == nil then i5 = 0 end
    local out = C.ImDrawList_AddCircleFilled(i1, i2, i3, i4, i5)
    return out
end
ImDrawList["AddImageQuad"] = ImDrawList["AddImageQuad"]  or function(i1, i2, i3, i4, i5, i6, i7, i8, i9, i10, i11)
    jit.off(true)
    if i7 == nil then i7 = M.ImVec2_Float(0, 0) end
    if i8 == nil then i8 = M.ImVec2_Float(1, 0) end
    if i9 == nil then i9 = M.ImVec2_Float(1, 1) end
    if i10 == nil then i10 = M.ImVec2_Float(0, 1) end
    if i11 == nil then i11 = 4294967295 end

    local out = C.ImDrawList_AddImageQuad(i1, i2, i3, i4, i5, i6, i7, i8, i9, i10, i11)
    return out
end
ImDrawList["AddLine"] = ImDrawList["AddLine"]  or function(i1, i2, i3, i4, i5)
    jit.off(true)
    if i5 == nil then i5 = 1.0 end
    local out = C.ImDrawList_AddLine(i1, i2, i3, i4, i5)
    return out
end
ImDrawList["AddQuadFilled"] = ImDrawList["AddQuadFilled"]  or function(i1, i2, i3, i4, i5, i6)
    jit.off(true)
    local out = C.ImDrawList_AddQuadFilled(i1, i2, i3, i4, i5, i6)
    return out
end
ImDrawList["AddRectFilled"] = ImDrawList["AddRectFilled"]  or function(i1, i2, i3, i4, i5, i6)
    jit.off(true)
    if i5 == nil then i5 = 0.0 end
    if i6 == nil then i6 = 0 end
    local out = C.ImDrawList_AddRectFilled(i1, i2, i3, i4, i5, i6)
    return out
end
M.ImDrawList = ImDrawList
ffi.metatype("ImDrawList", ImDrawList)

local ImGuiStyle = ImGuiStyle or {}
ImGuiStyle.__index = ImGuiStyle
ImGuiStyle["ScaleAllSizes"] = ImGuiStyle["ScaleAllSizes"]  or function(i1, i2)
    jit.off(true)
    local out = C.ImGuiStyle_ScaleAllSizes(i1, i2)
    return out
end
M.ImGuiStyle = ImGuiStyle
ffi.metatype("ImGuiStyle", ImGuiStyle)

local ImVec2 = ImVec2 or {}
ImVec2.__index = ImVec2
M.ImVec2_Nil = M.ImVec2_Nil  or function()
    jit.off(true)
    local p = C.ImVec2_ImVec2_Nil()
    return ffi.gc(p[0], C.ImVec2_destroy)
end
M.ImVec2_Float = M.ImVec2_Float  or function(i1, i2)
    jit.off(true)
    local p = C.ImVec2_ImVec2_Float(i1, i2)
    return ffi.gc(p[0], C.ImVec2_destroy)
end
M.ImVec2 = ImVec2
ffi.metatype("ImVec2", ImVec2)

local ImVec4 = ImVec4 or {}
ImVec4.__index = ImVec4
M.ImVec4 = ImVec4
ffi.metatype("ImVec4", ImVec4)

M.Begin = M.Begin  or function(i1, i2, i3)
    jit.off(true)
    if i3 == nil then i3 = 0 end
    local out = C.igBegin(i1, i2, i3)
    return out
end
M.BeginChild_Str = M.BeginChild_Str  or function(i1, i2, i3, i4)
    jit.off(true)
    if i2 == nil then i2 = M.ImVec2_Float(0, 0) end
    if i3 == nil then i3 = 0 end
    if i4 == nil then i4 = 0 end
    local out = C.igBeginChild_Str(i1, i2, i3, i4)
    return out
end
M.BeginCombo = M.BeginCombo  or function(i1, i2, i3)
    jit.off(true)
    if i3 == nil then i3 = 0 end
    local out = C.igBeginCombo(i1, i2, i3)
    return out
end
M.BeginDisabled = M.BeginDisabled  or function(i1)
    jit.off(true)
    if i1 == nil then i1 = true end
    local out = C.igBeginDisabled(i1)
    return out
end
M.BeginTable = M.BeginTable  or function(i1, i2, i3, i4, i5)
    jit.off(true)
    if i3 == nil then i3 = 0 end
    if i4 == nil then i4 = M.ImVec2_Float(0.0, 0.0) end
    if i5 == nil then i5 = 0.0 end
    local out = C.igBeginTable(i1, i2, i3, i4, i5)
    return out
end
M.Button = M.Button  or function(i1, i2)
    jit.off(true)
    if i2 == nil then i2 = M.ImVec2_Float(0, 0) end
    local out = C.igButton(i1, i2)
    return out
end
M.Checkbox = M.Checkbox  or function(i1, i2)
    jit.off(true)
    local out = C.igCheckbox(i1, i2)
    return out
end
M.ClearActiveID = M.ClearActiveID  or function()
    jit.off(true)
    local out = C.igClearActiveID()
    return out
end
M.ClearIniSettings = M.ClearIniSettings  or function()
    jit.off(true)
    local out = C.igClearIniSettings()
    return out
end
M.CollapsingHeader_TreeNodeFlags = M.CollapsingHeader_TreeNodeFlags  or function(i1, i2)
    jit.off(true)
    if i2 == nil then i2 = 0 end
    local out = C.igCollapsingHeader_TreeNodeFlags(i1, i2)
    return out
end
M.ColorEdit4 = M.ColorEdit4  or function(i1, i2, i3)
    jit.off(true)
    if i3 == nil then i3 = 0 end
    local out = C.igColorEdit4(i1, i2, i3)
    return out
end
M.ColorPicker4 = M.ColorPicker4  or function(i1, i2, i3, i4)
    jit.off(true)
    if i3 == nil then i3 = 0 end
    local out = C.igColorPicker4(i1, i2, i3, i4)
    return out
end
M.Combo_Str = M.Combo_Str  or function(i1, i2, i3, i4)
    jit.off(true)
    if i4 == nil then i4 = -1 end
    local out = C.igCombo_Str(i1, i2, i3, i4)
    return out
end
M.CreateContext = M.CreateContext  or function(i1)
    jit.off(true)
    local out = C.igCreateContext(i1)
    return out
end
M.DestroyContext = M.DestroyContext  or function(i1)
    jit.off(true)
    local out = C.igDestroyContext(i1)
    return out
end
M.DockBuilderAddNode = M.DockBuilderAddNode  or function(i1, i2)
    jit.off(true)
    if i1 == nil then i1 = 0 end
    if i2 == nil then i2 = 0 end
    local out = C.igDockBuilderAddNode(i1, i2)
    return out
end
M.DockBuilderDockWindow = M.DockBuilderDockWindow  or function(i1, i2)
    jit.off(true)
    local out = C.igDockBuilderDockWindow(i1, i2)
    return out
end
M.DockBuilderFinish = M.DockBuilderFinish  or function(i1)
    jit.off(true)
    local out = C.igDockBuilderFinish(i1)
    return out
end
M.DockBuilderGetNode = M.DockBuilderGetNode  or function(i1)
    jit.off(true)
    local out = C.igDockBuilderGetNode(i1)
    return out
end
M.DockBuilderRemoveNode = M.DockBuilderRemoveNode  or function(i1)
    jit.off(true)
    local out = C.igDockBuilderRemoveNode(i1)
    return out
end
M.DockBuilderSetNodeSize = M.DockBuilderSetNodeSize  or function(i1, i2)
    jit.off(true)
    local out = C.igDockBuilderSetNodeSize(i1, i2)
    return out
end
M.DockBuilderSplitNode = M.DockBuilderSplitNode  or function(i1, i2, i3)
    jit.off(true)
    local o1 = ffi.new("ImGuiID[1]")
    local o2 = ffi.new("ImGuiID[1]")
    local out = C.igDockBuilderSplitNode(i1, i2, i3, o1, o2)
    return o1[0], o2[0], out
end
M.DockSpaceOverViewport = M.DockSpaceOverViewport  or function(i1, i2, i3, i4)
    jit.off(true)
    if i1 == nil then i1 = 0 end
    if i3 == nil then i3 = 0 end
    local out = C.igDockSpaceOverViewport(i1, i2, i3, i4)
    return out
end
M.Dummy = M.Dummy  or function(i1)
    jit.off(true)
    local out = C.igDummy(i1)
    return out
end
M.End = M.End  or function()
    jit.off(true)
    local out = C.igEnd()
    return out
end
M.EndChild = M.EndChild  or function()
    jit.off(true)
    local out = C.igEndChild()
    return out
end
M.EndCombo = M.EndCombo  or function()
    jit.off(true)
    local out = C.igEndCombo()
    return out
end
M.EndDisabled = M.EndDisabled  or function()
    jit.off(true)
    local out = C.igEndDisabled()
    return out
end
M.EndFrame = M.EndFrame  or function()
    jit.off(true)
    local out = C.igEndFrame()
    return out
end
M.EndTable = M.EndTable  or function()
    jit.off(true)
    local out = C.igEndTable()
    return out
end
M.GetContentRegionAvail = M.GetContentRegionAvail  or function()
    jit.off(true)
    local o1 = M.ImVec2_Nil()
    local out = C.igGetContentRegionAvail(o1)
    return o1, out
end
M.GetCurrentContext = M.GetCurrentContext  or function()
    jit.off(true)
    local out = C.igGetCurrentContext()
    return out
end
M.GetDrawData = M.GetDrawData  or function()
    jit.off(true)
    local out = C.igGetDrawData()
    return out
end
M.GetForegroundDrawList_ViewportPtr = M.GetForegroundDrawList_ViewportPtr  or function(i1)
    jit.off(true)
    local out = C.igGetForegroundDrawList_ViewportPtr(i1)
    return out
end
M.GetItemRectMax = M.GetItemRectMax  or function()
    jit.off(true)
    local o1 = M.ImVec2_Nil()
    local out = C.igGetItemRectMax(o1)
    return o1, out
end
M.GetItemRectMin = M.GetItemRectMin  or function()
    jit.off(true)
    local o1 = M.ImVec2_Nil()
    local out = C.igGetItemRectMin(o1)
    return o1, out
end
M.GetMainViewport = M.GetMainViewport  or function()
    jit.off(true)
    local out = C.igGetMainViewport()
    return out
end
M.GetMouseDragDelta = M.GetMouseDragDelta  or function(i1, i2)
    jit.off(true)
    if i1 == nil then i1 = 0 end
    if i2 == nil then i2 = -1.0 end
    local o1 = M.ImVec2_Nil()
    local out = C.igGetMouseDragDelta(o1, i1, i2)
    return o1, out
end
M.GetMousePos = M.GetMousePos  or function()
    jit.off(true)
    local o1 = M.ImVec2_Nil()
    local out = C.igGetMousePos(o1)
    return o1, out
end
M.GetStyle = M.GetStyle  or function()
    jit.off(true)
    local out = C.igGetStyle()
    return out
end
M.GetWindowDrawList = M.GetWindowDrawList  or function()
    jit.off(true)
    local out = C.igGetWindowDrawList()
    return out
end
M.GetWindowPos = M.GetWindowPos  or function()
    jit.off(true)
    local o1 = M.ImVec2_Nil()
    local out = C.igGetWindowPos(o1)
    return o1, out
end
M.GetWindowSize = M.GetWindowSize  or function()
    jit.off(true)
    local o1 = M.ImVec2_Nil()
    local out = C.igGetWindowSize(o1)
    return o1, out
end
M.Image = M.Image  or function(i1, i2, i3, i4)
    jit.off(true)
    if i3 == nil then i3 = M.ImVec2_Float(0, 0) end
    if i4 == nil then i4 = M.ImVec2_Float(1, 1) end

    local out = C.igImage(i1, i2, i3, i4)
    return out
end
M.Indent = M.Indent  or function(i1)
    jit.off(true)
    if i1 == nil then i1 = 0.0 end
    local out = C.igIndent(i1)
    return out
end
M.InputText = M.InputText  or function(i1, i2, i3, i4, i5, i6)
    jit.off(true)
    if i4 == nil then i4 = 0 end
    local out = C.igInputText(i1, i2, i3, i4, i5, i6)
    return out
end
M.InputTextMultiline = M.InputTextMultiline  or function(i1, i2, i3, i4, i5, i6, i7)
    jit.off(true)
    if i4 == nil then i4 = M.ImVec2_Float(0, 0) end
    if i5 == nil then i5 = 0 end
    local out = C.igInputTextMultiline(i1, i2, i3, i4, i5, i6, i7)
    return out
end
M.InvisibleButton = M.InvisibleButton  or function(i1, i2, i3)
    jit.off(true)
    if i3 == nil then i3 = 0 end
    local out = C.igInvisibleButton(i1, i2, i3)
    return out
end
M.IsAnyItemActive = M.IsAnyItemActive  or function()
    jit.off(true)
    local out = C.igIsAnyItemActive()
    return out
end
M.IsItemActivated = M.IsItemActivated  or function()
    jit.off(true)
    local out = C.igIsItemActivated()
    return out
end
M.IsItemActive = M.IsItemActive  or function()
    jit.off(true)
    local out = C.igIsItemActive()
    return out
end
M.IsItemDeactivated = M.IsItemDeactivated  or function()
    jit.off(true)
    local out = C.igIsItemDeactivated()
    return out
end
M.IsItemDeactivatedAfterEdit = M.IsItemDeactivatedAfterEdit  or function()
    jit.off(true)
    local out = C.igIsItemDeactivatedAfterEdit()
    return out
end
M.IsItemHovered = M.IsItemHovered  or function(i1)
    jit.off(true)
    if i1 == nil then i1 = 0 end
    local out = C.igIsItemHovered(i1)
    return out
end
M.IsItemVisible = M.IsItemVisible  or function()
    jit.off(true)
    local out = C.igIsItemVisible()
    return out
end
M.IsKeyPressed_Bool = M.IsKeyPressed_Bool  or function(i1, i2)
    jit.off(true)
    if i2 == nil then i2 = true end
    local out = C.igIsKeyPressed_Bool(i1, i2)
    return out
end
M.IsMouseDown_Nil = M.IsMouseDown_Nil  or function(i1)
    jit.off(true)
    local out = C.igIsMouseDown_Nil(i1)
    return out
end
M.IsRectVisible_Nil = M.IsRectVisible_Nil  or function(i1)
    jit.off(true)
    local out = C.igIsRectVisible_Nil(i1)
    return out
end
M.IsRectVisible_Vec2 = M.IsRectVisible_Vec2  or function(i1, i2)
    jit.off(true)
    local out = C.igIsRectVisible_Vec2(i1, i2)
    return out
end
M.IsWindowFocused = M.IsWindowFocused  or function(i1)
    jit.off(true)
    if i1 == nil then i1 = 0 end
    local out = C.igIsWindowFocused(i1)
    return out
end
M.IsWindowHovered = M.IsWindowHovered  or function(i1)
    jit.off(true)
    if i1 == nil then i1 = 0 end
    local out = C.igIsWindowHovered(i1)
    return out
end
M.NewFrame = M.NewFrame  or function()
    jit.off(true)
    local out = C.igNewFrame()
    return out
end
M.PopStyleColor = M.PopStyleColor  or function(i1)
    jit.off(true)
    if i1 == nil then i1 = 1 end
    local out = C.igPopStyleColor(i1)
    return out
end
M.PopTextWrapPos = M.PopTextWrapPos  or function()
    jit.off(true)
    local out = C.igPopTextWrapPos()
    return out
end
M.PushStyleColor_U32 = M.PushStyleColor_U32  or function(i1, i2)
    jit.off(true)
    local out = C.igPushStyleColor_U32(i1, i2)
    return out
end
M.PushTextWrapPos = M.PushTextWrapPos  or function(i1)
    jit.off(true)
    if i1 == nil then i1 = 0.0 end
    local out = C.igPushTextWrapPos(i1)
    return out
end
M.Render = M.Render  or function()
    jit.off(true)
    local out = C.igRender()
    return out
end
M.SameLine = M.SameLine  or function(i1, i2)
    jit.off(true)
    if i1 == nil then i1 = 0.0 end
    if i2 == nil then i2 = -1.0 end
    local out = C.igSameLine(i1, i2)
    return out
end
M.Selectable_Bool = M.Selectable_Bool  or function(i1, i2, i3, i4)
    jit.off(true)
    if i2 == nil then i2 = false end
    if i3 == nil then i3 = 0 end
    if i4 == nil then i4 = M.ImVec2_Float(0, 0) end
    local out = C.igSelectable_Bool(i1, i2, i3, i4)
    return out
end
M.Separator = M.Separator  or function()
    jit.off(true)
    local out = C.igSeparator()
    return out
end
M.SetCurrentContext = M.SetCurrentContext  or function(i1)
    jit.off(true)
    local out = C.igSetCurrentContext(i1)
    return out
end
M.SetCursorScreenPos = M.SetCursorScreenPos  or function(i1)
    jit.off(true)
    local out = C.igSetCursorScreenPos(i1)
    return out
end
M.SetItemDefaultFocus = M.SetItemDefaultFocus  or function()
    jit.off(true)
    local out = C.igSetItemDefaultFocus()
    return out
end
M.SetKeyboardFocusHere = M.SetKeyboardFocusHere  or function(i1)
    jit.off(true)
    if i1 == nil then i1 = 0 end
    local out = C.igSetKeyboardFocusHere(i1)
    return out
end
M.SetNextItemWidth = M.SetNextItemWidth  or function(i1)
    jit.off(true)
    local out = C.igSetNextItemWidth(i1)
    return out
end
M.SetNextWindowDockID = M.SetNextWindowDockID  or function(i1, i2)
    jit.off(true)
    if i2 == nil then i2 = 0 end
    local out = C.igSetNextWindowDockID(i1, i2)
    return out
end
M.SetNextWindowPos = M.SetNextWindowPos  or function(i1, i2, i3)
    jit.off(true)
    if i2 == nil then i2 = 0 end
    if i3 == nil then i3 = M.ImVec2_Float(0, 0) end
    local out = C.igSetNextWindowPos(i1, i2, i3)
    return out
end
M.SetNextWindowSize = M.SetNextWindowSize  or function(i1, i2)
    jit.off(true)
    if i2 == nil then i2 = 0 end
    local out = C.igSetNextWindowSize(i1, i2)
    return out
end
M.SetNextWindowSizeConstraints = M.SetNextWindowSizeConstraints  or function(i1, i2, i3, i4)
    jit.off(true)
    local out = C.igSetNextWindowSizeConstraints(i1, i2, i3, i4)
    return out
end
M.SetScrollHereY = M.SetScrollHereY  or function(i1)
    jit.off(true)
    if i1 == nil then i1 = 0.5 end
    local out = C.igSetScrollHereY(i1)
    return out
end
M.SetTooltip = M.SetTooltip  or function(i1, ...)
    jit.off(true)
    local out = C.igSetTooltip(i1, ...)
    return out
end
M.SliderFloat = M.SliderFloat  or function(i1, i2, i3, i4, i5, i6)
    jit.off(true)
    if i5 == nil then i5 = "%.3f" end
    if i6 == nil then i6 = 0 end
    local out = C.igSliderFloat(i1, i2, i3, i4, i5, i6)
    return out
end
M.SliderFloat2 = M.SliderFloat2  or function(i1, i2, i3, i4, i5, i6)
    jit.off(true)
    if i5 == nil then i5 = "%.3f" end
    if i6 == nil then i6 = 0 end
    local out = C.igSliderFloat2(i1, i2, i3, i4, i5, i6)
    return out
end
M.SliderFloat4 = M.SliderFloat4  or function(i1, i2, i3, i4, i5, i6)
    jit.off(true)
    if i5 == nil then i5 = "%.3f" end
    if i6 == nil then i6 = 0 end
    local out = C.igSliderFloat4(i1, i2, i3, i4, i5, i6)
    return out
end
M.SliderInt = M.SliderInt  or function(i1, i2, i3, i4, i5, i6)
    jit.off(true)
    if i5 == nil then i5 = "%d" end
    if i6 == nil then i6 = 0 end
    local out = C.igSliderInt(i1, i2, i3, i4, i5, i6)
    return out
end
M.TableHeadersRow = M.TableHeadersRow  or function()
    jit.off(true)
    local out = C.igTableHeadersRow()
    return out
end
M.TableNextColumn = M.TableNextColumn  or function()
    jit.off(true)
    local out = C.igTableNextColumn()
    return out
end
M.TableNextRow = M.TableNextRow  or function(i1, i2)
    jit.off(true)
    if i1 == nil then i1 = 0 end
    if i2 == nil then i2 = 0.0 end
    local out = C.igTableNextRow(i1, i2)
    return out
end
M.TableSetupColumn = M.TableSetupColumn  or function(i1, i2, i3, i4)
    jit.off(true)
    if i2 == nil then i2 = 0 end
    if i3 == nil then i3 = 0.0 end
    if i4 == nil then i4 = 0 end
    local out = C.igTableSetupColumn(i1, i2, i3, i4)
    return out
end
M.TextColored = M.TextColored  or function(i1, i2, ...)
    jit.off(true)
    local out = C.igTextColored(i1, i2, ...)
    return out
end
M.TextUnformatted = M.TextUnformatted  or function(i1, i2)
    jit.off(true)
    local out = C.igTextUnformatted(i1, i2)
    return out
end
M.TreeNode_Str = M.TreeNode_Str  or function(i1)
    jit.off(true)
    local out = C.igTreeNode_Str(i1)
    return out
end
M.TreePop = M.TreePop  or function()
    jit.off(true)
    local out = C.igTreePop()
    return out
end
M.Unindent = M.Unindent  or function(i1)
    jit.off(true)
    if i1 == nil then i1 = 0.0 end
    local out = C.igUnindent(i1)
    return out
end
M.VSliderFloat = M.VSliderFloat  or function(i1, i2, i3, i4, i5, i6, i7)
    jit.off(true)
    if i6 == nil then i6 = "%.3f" end
    if i7 == nil then i7 = 0 end
    local out = C.igVSliderFloat(i1, i2, i3, i4, i5, i6, i7)
    return out
end
