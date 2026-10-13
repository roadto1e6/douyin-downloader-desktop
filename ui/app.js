'use strict';
const $ = selector => document.querySelector(selector);
const invoke = (command, args) => window.__TAURI__.core.invoke(command, args);
let running = false, submitting = false, polling = false, cancelling = false;
let lastFiles = '', lastStatus = '', startedAt = 0, toastTimer;
const modeNames = { both: '下载视频与音频', video: '下载视频', audio: '下载音频' };

function icon(id) {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  const use = document.createElementNS('http://www.w3.org/2000/svg', 'use');
  use.setAttribute('href', '#' + id); svg.append(use); return svg;
}
function toast(message) {
  clearTimeout(toastTimer); $('#toast').textContent = String(message); $('#toast').hidden = false;
  toastTimer = setTimeout(() => { $('#toast').hidden = true; }, 4500);
}
function validLink(text) {
  const matches = text.match(/https:\/\/[^\s<>"\u3000]+/g) || [];
  return matches.some(raw => {
    try {
      const url = new URL(raw.replace(/[，。；！）】,;!)\]]+$/, ''));
      return ['v.douyin.com', 'www.douyin.com', 'douyin.com', 'www.iesdouyin.com', 'iesdouyin.com'].includes(url.hostname)
        && !url.username && !url.password && (!url.port || url.port === '443');
    } catch { return false; }
  });
}
function note(message, type = '') {
  $('#input-note').textContent = message; $('#input-note').className = 'input-note ' + type;
  $('.input-wrap').classList.toggle('invalid', type === 'invalid');
  $('#link').setAttribute('aria-invalid', String(type === 'invalid'));
}
function inputChanged() {
  const text = $('#link').value.trim(); $('#clear').hidden = !text;
  if (!text) note('支持短链接与视频作品链接');
  else if (validLink(text)) note('已识别抖音链接，可以开始保存', 'valid');
  else note('支持直接粘贴整段分享文字');
}
function controls() {
  const busy = running || submitting;
  $('#start').disabled = busy; $('#paste').disabled = busy; $('#choose').disabled = busy;
  $('#clear').disabled = busy; $('#link').disabled = busy;
  document.querySelectorAll('input[name=mode]').forEach(input => { input.disabled = busy; });
  $('#start-text').textContent = cancelling ? '正在取消…' : busy ? '正在保存…' : modeNames[$('input[name=mode]:checked').value];
  $('.button-spinner').hidden = !busy; $('.download-icon').hidden = busy;
  $('#cancel').disabled = cancelling; $('.shortcut').hidden = busy;
}
function renderFiles(files) {
  const key = JSON.stringify(files); if (key === lastFiles) return; lastFiles = key;
  $('#files').replaceChildren(...files.map((path, index) => {
    const name = path.split(/[\\/]/).pop(), audio = /\.mp3$/i.test(name);
    const li = document.createElement('li'), badge = document.createElement('span');
    badge.className = 'file-icon'; badge.append(icon(audio ? 'i-audio' : 'i-video'));
    const info = document.createElement('span'); info.className = 'file-info';
    const label = document.createElement('span'); label.className = 'file-name'; label.textContent = name; label.title = name;
    const type = document.createElement('span'); type.className = 'file-type'; type.textContent = audio ? 'MP3 · 音频' : 'MP4 · 视频';
    info.append(label, type);
    const open = document.createElement('button'); open.className = 'file-open'; open.type = 'button';
    open.title = '打开' + (audio ? '音频' : '视频'); open.setAttribute('aria-label', '打开文件：' + name); open.append(icon('i-open'));
    open.addEventListener('click', async () => { try { await invoke('open_file', { index }); } catch (error) { toast(error); } });
    li.append(badge, info, open); return li;
  }));
}
function render(state) {
  running = state.status === 'running';
  if (running && !startedAt) startedAt = Date.now();
  if (!running) { startedAt = 0; cancelling = false; }
  $('.transfer').dataset.state = state.status;
  $('#folder').textContent = state.output; $('#folder').title = state.output;
  const labels = { idle: '准备就绪', running: '正在处理', done: '已完成', error: '下载失败', cancelled: '已取消' };
  $('#label').textContent = cancelling ? '正在取消' : labels[state.status] || '准备就绪';
  const titles = { idle: '等待一个好片段', running: '正在为你收好', done: '喜欢的，已经留下。', error: '这次没能下载', cancelled: '任务已取消' };
  $('#status-title').textContent = cancelling ? '正在收尾清理' : titles[state.status] || titles.idle;
  const messages = {
    idle: '在左侧粘贴链接，其余的交给我们。',
    done: '已保存 ' + state.files.length + ' 个文件，随时打开重温。',
    error: '请检查链接能否公开播放，或稍后重试。',
    cancelled: '临时文件已清理，可以重新开始。'
  };
  $('#message').textContent = cancelling ? '等待当前操作结束后，将清理临时文件。' : running ? state.message : messages[state.status] || state.message;
  $('#progress').value = state.progress; $('#percent').textContent = state.progress + '%';
  $('.progress-area').hidden = !running;
  const step = state.progress < 30 ? 0 : state.progress < 78 ? 1 : 2;
  $('#stage-name').textContent = ['正在解析作品', '正在下载视频', '正在处理文件'][step];
  document.querySelectorAll('.stages li').forEach((li, index) => {
    li.classList.toggle('active', index === step); li.classList.toggle('complete', index < step);
    if (index === step) li.setAttribute('aria-current', 'step'); else li.removeAttribute('aria-current');
  });
  renderFiles(state.files);
  $('#error-details').hidden = state.status !== 'error';
  if (lastStatus !== state.status) $('#error-details').open = false;
  $('#error-message').textContent = state.status === 'error' ? state.message : '';
  $('#privacy-note').hidden = running || state.status === 'done';
  $('#open').hidden = state.status !== 'done'; $('#cancel').hidden = !running; $('#elapsed').hidden = !running;
  lastStatus = state.status; controls();
}
async function refresh() {
  if (polling) return; polling = true;
  try { render(await invoke('get_state')); } catch (error) { toast('无法读取任务状态，请重启工具。'); }
  finally { polling = false; }
}
async function cancel() {
  if (!running || cancelling) return;
  cancelling = true; controls();
  try { await invoke('cancel_download'); await refresh(); } catch (error) { cancelling = false; controls(); toast(error); }
}
$('#form').addEventListener('submit', async event => {
  event.preventDefault(); if (running || submitting) return;
  const link = $('#link').value.trim();
  if (!link || !validLink(link)) { note(link ? '没有找到有效的抖音链接，请检查后重试' : '请先粘贴一个抖音链接', 'invalid'); $('#link').focus(); return; }
  submitting = true; controls();
  try {
    await invoke('start_download', { link, mode: $('input[name=mode]:checked').value });
    running = true; startedAt = Date.now(); await refresh();
  } catch (error) { note(String(error), 'invalid'); }
  finally { submitting = false; controls(); }
});
$('#link').addEventListener('input', inputChanged);
document.querySelectorAll('input[name=mode]').forEach(input => input.addEventListener('change', controls));
$('#clear').addEventListener('click', () => { $('#link').value = ''; inputChanged(); $('#link').focus(); });
$('#paste').addEventListener('click', async () => {
  try {
    const text = await invoke('read_clipboard');
    if (!text.trim()) { toast('剪贴板里还没有文字，先复制一条分享链接吧。'); return; }
    $('#link').value = text.slice(0, 16384); inputChanged(); $('#link').focus();
  } catch (error) { toast(error); }
});
$('#choose').addEventListener('click', async () => { try { await invoke('choose_folder'); await refresh(); } catch (error) { toast(error); } });
$('#cancel').addEventListener('click', cancel);
$('#open').addEventListener('click', async () => { try { await invoke('open_folder'); } catch (error) { toast(error); } });
document.addEventListener('keydown', event => {
  if ((event.ctrlKey || event.metaKey) && event.key === 'Enter') { event.preventDefault(); if (!running && !submitting) $('#form').requestSubmit(); }
  if (event.key === 'Escape' && running) { event.preventDefault(); cancel(); }
});
refresh();
setInterval(() => {
  if (running || submitting) refresh();
  if (startedAt) { const seconds = Math.floor((Date.now() - startedAt) / 1000); $('#elapsed').textContent = String(Math.floor(seconds / 60)).padStart(2, '0') + ':' + String(seconds % 60).padStart(2, '0'); }
}, 500);
