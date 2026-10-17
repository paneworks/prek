/repl (function()
local function quote(s)
 assert(type(s)=="string" and #s<=256)
 local out={'"'}
 for i=1,#s do
  local byte=string.byte(s,i)
  if byte<32 then out[#out+1]=string.format('\\u%04x',byte)
  elseif byte==34 or byte==92 then out[#out+1]='\\'..string.char(byte)
  else out[#out+1]=string.char(byte) end
 end
 out[#out+1]='"'
 return table.concat(out)
end
local function identity(w)
 return quote(w.address..':'..string.format('%x',w.stable_id))
end
local workspaces=hl.get_workspaces()
assert(#workspaces<=64)
local result,total,member_total={},0,0
for _,ws in ipairs(workspaces) do
 local monitor=ws.monitor
 local windows=hl.get_workspace_windows(ws)
 total=total+#windows
 assert(total<=512)
 local rows,columns,seen={},{},{}
 for _,w in ipairs(windows) do
  local layout=w.layout
  local col=layout and layout.column
  local position='null'
  if col then
   position=string.format('{"index":%d,"row":%d}',col.index,layout.index_in_column)
   if not seen[col.index] then
    seen[col.index]=true
    local members={}
    assert(#col.windows<=512)
    member_total=member_total+#col.windows
    assert(member_total<=512)
    for _,member in ipairs(col.windows) do members[#members+1]=identity(member) end
    columns[#columns+1]={index=col.index,value=string.format(
     '{"index":%d,"width":%.17g,"members":[%s]}',col.index,col.width,table.concat(members,','))}
   end
  end
  local p,s=w.at,w.size
  rows[#rows+1]=string.format(
   '{"id":%s,"rect":[%d,%d,%d,%d],"hidden":%s,"floating":%s,"grouped":%s,"fullscreen":%d,"column":%s}',
   identity(w),p.x,p.y,s.x,s.y,tostring(w.hidden),tostring(w.floating),tostring(w.group~=nil),w.fullscreen,position)
 end
 table.sort(columns,function(a,b)return a.index<b.index end)
 local cols={}
 for _,col in ipairs(columns) do cols[#cols+1]=col.value end
 result[#result+1]=string.format(
  '{"id":%d,"output":%s,"monitor":%s,"active":%s,"special":%s,"layout":%s,"windows":[%s],"columns":[%s]}',
  ws.id,monitor and quote(monitor.name) or 'null',monitor and tostring(monitor.id) or 'null',
  tostring(ws.active),tostring(ws.special),quote(ws.tiled_layout),table.concat(rows,','),table.concat(cols,','))
end
return '{"schema":1,"workspaces":['..table.concat(result,',')..']}'
end)()
