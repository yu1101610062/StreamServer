import 'dart:convert';

import 'package:flutter/material.dart';

import '../state.dart';
import '../utils.dart';
import '../widgets/data_panel.dart';

const _multicastInputKinds = {
  'udp_mpegts_multicast',
  'rtp_multicast',
};
const _portInputKinds = {
  'udp_mpegts_multicast',
  'rtp_multicast',
  'gb_rtp',
};

bool taskInputUsesUrl(String kind) =>
    !_multicastInputKinds.contains(kind) && kind != 'gb_rtp';

bool taskInputUsesGroup(String kind) => _multicastInputKinds.contains(kind);

bool taskInputUsesPort(String kind) => _portInputKinds.contains(kind);

Map<String, Object?> buildTaskInputPayload({
  required String inputKind,
  required String sourceMode,
  required bool loopEnabled,
  required String url,
  required String group,
  required String port,
  required String interfaceName,
  required String interfaceIp,
  required String ttl,
}) {
  return cleanTaskPayloadMap({
    'kind': inputKind,
    'source_mode': sourceMode,
    'loop_enabled': loopEnabled,
    if (taskInputUsesUrl(inputKind)) 'url': url,
    if (taskInputUsesGroup(inputKind)) 'group': group,
    if (taskInputUsesPort(inputKind)) 'port': int.tryParse(port.trim()),
    'interface_name': interfaceName,
    'interface_ip': interfaceIp,
    'ttl': int.tryParse(ttl.trim()),
  });
}

Map<String, Object?> cleanTaskPayloadMap(Map<String, Object?> map) {
  final next = <String, Object?>{};
  for (final entry in map.entries) {
    final value = entry.value;
    if (value == null) continue;
    if (value is String && value.trim().isEmpty) continue;
    if (value is List && value.isEmpty) continue;
    if (value is Map<String, Object?>) {
      final cleaned = cleanTaskPayloadMap(value);
      if (cleaned.isNotEmpty) next[entry.key] = cleaned;
      continue;
    }
    next[entry.key] = value;
  }
  return next;
}

class TaskCreateScreen extends StatefulWidget {
  const TaskCreateScreen({super.key});

  @override
  State<TaskCreateScreen> createState() => _TaskCreateScreenState();
}

class _TaskCreateScreenState extends State<TaskCreateScreen> {
  final nameController = TextEditingController();
  final sourceController = TextEditingController();
  final streamAppController = TextEditingController(text: 'live');
  final streamNameController = TextEditingController();
  final vhostController = TextEditingController(text: '__defaultVhost__');
  final priorityController = TextEditingController(text: '50');
  final createdByController = TextEditingController(text: 'desktop');
  final callbackController = TextEditingController();
  final labelsController = TextEditingController();
  final publishUrlController = TextEditingController();
  final inputGroupController = TextEditingController();
  final inputPortController = TextEditingController();
  final publishGroupController = TextEditingController();
  final publishPortController = TextEditingController();
  final interfaceNameController = TextEditingController();
  final interfaceIpController = TextEditingController();
  final ttlController = TextEditingController();
  final durationController = TextEditingController();
  final segmentController = TextEditingController();
  final requiredLabelsController = TextEditingController();
  final startAtController = TextEditingController();
  final cronController = TextEditingController();
  final expertController = TextEditingController();

  String scenario = 'live-ingest';
  String taskType = 'stream_ingest';
  String inputKind = 'rtsp';
  String sourceMode = 'live';
  String processMode = 'copy_or_transcode';
  String publishKind = '';
  String publishFormat = '';
  String recordFormat = 'mp4';
  String recoveryPolicy = 'auto';
  String startMode = 'immediate';
  bool loopEnabled = false;
  bool enableRtsp = true;
  bool enableRtmp = true;
  bool enableHttpTs = true;
  bool enableHttpFmp4 = false;
  bool enableHls = false;
  bool recordEnabled = false;
  bool recordAsPlayer = false;
  String? result;

  @override
  void dispose() {
    for (final controller in [
      nameController,
      sourceController,
      streamAppController,
      streamNameController,
      vhostController,
      priorityController,
      createdByController,
      callbackController,
      labelsController,
      publishUrlController,
      inputGroupController,
      inputPortController,
      publishGroupController,
      publishPortController,
      interfaceNameController,
      interfaceIpController,
      ttlController,
      durationController,
      segmentController,
      requiredLabelsController,
      startAtController,
      cronController,
      expertController,
    ]) {
      controller.dispose();
    }
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        const PageHeader(
          title: '新建任务',
          description: '覆盖输入、处理、协议暴露、发布、录制、恢复、调度和资源标签；专家模式仍可直接提交 JSON。',
        ),
        Surface(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              SegmentedButton<String>(
                segments: const [
                  ButtonSegment(value: 'live-ingest', label: Text('实时流接入')),
                  ButtonSegment(value: 'ingest-record', label: Text('接入并录制')),
                  ButtonSegment(value: 'bridge-out', label: Text('桥接输出')),
                  ButtonSegment(value: 'file-transcode', label: Text('离线转码')),
                  ButtonSegment(value: 'expert', label: Text('专家 JSON')),
                ],
                selected: {scenario},
                onSelectionChanged: (value) => _applyScenario(value.first),
              ),
              const SizedBox(height: 16),
              if (scenario == 'expert')
                TextField(
                  controller: expertController,
                  minLines: 16,
                  maxLines: 28,
                  decoration: const InputDecoration(labelText: '任务 JSON'),
                )
              else
                Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    const _SectionTitle('基础信息'),
                    _Grid([
                      _TextFieldBox('任务名称', nameController),
                      _TextFieldBox('优先级', priorityController),
                      _TextFieldBox('创建者', createdByController),
                      _TextFieldBox('回调 URL', callbackController, width: 420),
                      _TextFieldBox('业务标签，逗号分隔', labelsController, width: 420),
                    ]),
                    const _SectionTitle('输入与处理'),
                    _Grid([
                      _SelectBox(
                          '任务类型',
                          taskType,
                          ['stream_ingest', 'stream_bridge', 'file_transcode'],
                          (value) => setState(() => taskType = value)),
                      _SelectBox(
                          '输入类型',
                          inputKind,
                          [
                            'rtsp',
                            'rtmp',
                            'hls',
                            'http_flv',
                            'http_ts',
                            'http_mp4',
                            'ftp',
                            'file',
                            'udp_mpegts_multicast',
                            'rtp_multicast',
                            'gb_rtp'
                          ],
                          (value) => setState(() => inputKind = value)),
                      _SelectBox('源模式', sourceMode, ['live', 'vod'],
                          (value) => setState(() => sourceMode = value)),
                      _SelectBox(
                          '处理模式',
                          processMode,
                          ['copy_or_transcode', 'copy', 'transcode'],
                          (value) => setState(() => processMode = value)),
                      if (taskInputUsesUrl(inputKind))
                        _TextFieldBox('输入 URL / 文件路径', sourceController,
                            width: 520),
                      if (taskInputUsesGroup(inputKind))
                        _TextFieldBox('输入组播地址', inputGroupController),
                      if (taskInputUsesPort(inputKind))
                        _TextFieldBox(
                            inputKind == 'gb_rtp' ? 'GB RTP 监听端口' : '输入端口',
                            inputPortController),
                      _SwitchBox('循环 VOD', loopEnabled,
                          (value) => setState(() => loopEnabled = value)),
                    ]),
                    const _SectionTitle('内部流与播放协议'),
                    _Grid([
                      _TextFieldBox('App', streamAppController),
                      _TextFieldBox('Stream', streamNameController),
                      _TextFieldBox('Vhost', vhostController),
                      _SwitchBox('RTSP', enableRtsp,
                          (value) => setState(() => enableRtsp = value)),
                      _SwitchBox('RTMP', enableRtmp,
                          (value) => setState(() => enableRtmp = value)),
                      _SwitchBox('HTTP-TS', enableHttpTs,
                          (value) => setState(() => enableHttpTs = value)),
                      _SwitchBox('HTTP-FMP4', enableHttpFmp4,
                          (value) => setState(() => enableHttpFmp4 = value)),
                      _SwitchBox('HLS', enableHls,
                          (value) => setState(() => enableHls = value)),
                    ]),
                    const _SectionTitle('发布与网络'),
                    _Grid([
                      _SelectBox(
                          '发布类型',
                          publishKind,
                          [
                            '',
                            'file',
                            'udp_mpegts_multicast',
                            'rtp_multicast',
                            'rtmp_push'
                          ],
                          (value) => setState(() => publishKind = value)),
                      _SelectBox(
                          '文件格式',
                          publishFormat,
                          ['', 'mp4', 'hls', 'mpegts', 'flv'],
                          (value) => setState(() => publishFormat = value)),
                      _TextFieldBox('发布 URL / 文件路径', publishUrlController,
                          width: 420),
                      _TextFieldBox('组播地址', publishGroupController),
                      _TextFieldBox('端口', publishPortController),
                      _TextFieldBox('网卡名', interfaceNameController),
                      _TextFieldBox('绑定 IP', interfaceIpController),
                      _TextFieldBox('TTL', ttlController),
                    ]),
                    const _SectionTitle('录制、恢复与调度'),
                    _Grid([
                      _SwitchBox('启用录制', recordEnabled,
                          (value) => setState(() => recordEnabled = value)),
                      _SelectBox('录制格式', recordFormat, ['mp4', 'hls', 'both'],
                          (value) => setState(() => recordFormat = value)),
                      _TextFieldBox('录制时长秒', durationController),
                      _TextFieldBox('分段秒', segmentController),
                      _SwitchBox('按播放器模式录制', recordAsPlayer,
                          (value) => setState(() => recordAsPlayer = value)),
                      _SelectBox('恢复策略', recoveryPolicy, ['auto', 'none'],
                          (value) => setState(() => recoveryPolicy = value)),
                      _SelectBox(
                          '启动模式',
                          startMode,
                          ['immediate', 'manual', 'at', 'cron'],
                          (value) => setState(() => startMode = value)),
                      _TextFieldBox('启动时间 RFC3339', startAtController),
                      _TextFieldBox('Cron', cronController),
                      _TextFieldBox('节点标签要求，逗号分隔', requiredLabelsController,
                          width: 420),
                    ]),
                  ],
                ),
              const SizedBox(height: 16),
              Wrap(
                spacing: 8,
                children: [
                  OutlinedButton.icon(
                    onPressed: () => _run(context, () => _preview(controller)),
                    icon: const Icon(Icons.visibility),
                    label: const Text('规格预览'),
                  ),
                  FilledButton.icon(
                    onPressed: () => _run(context, () => _create(controller)),
                    icon: const Icon(Icons.playlist_add),
                    label: const Text('创建任务'),
                  ),
                ],
              ),
              if (result != null) ...[
                const SizedBox(height: 16),
                SelectableText(result!),
              ],
            ],
          ),
        ),
      ],
    );
  }

  void _applyScenario(String value) {
    setState(() {
      scenario = value;
      if (value == 'live-ingest') {
        taskType = 'stream_ingest';
        inputKind = 'rtsp';
        sourceMode = 'live';
        publishKind = '';
        recordEnabled = false;
      } else if (value == 'ingest-record') {
        taskType = 'stream_ingest';
        inputKind = 'rtsp';
        sourceMode = 'live';
        publishKind = '';
        recordEnabled = true;
      } else if (value == 'bridge-out') {
        taskType = 'stream_bridge';
        inputKind = 'rtsp';
        sourceMode = 'live';
        publishKind = 'file';
        publishFormat = 'mp4';
        recordEnabled = false;
      } else if (value == 'file-transcode') {
        taskType = 'file_transcode';
        inputKind = 'file';
        sourceMode = 'vod';
        publishKind = 'file';
        publishFormat = 'mp4';
        recordEnabled = false;
      }
    });
  }

  Map<String, Object?> _payload() {
    if (scenario == 'expert') {
      return (jsonDecode(expertController.text) as Map).cast<String, Object?>();
    }
    final name = nameController.text.trim().isEmpty
        ? 'desktop-${DateTime.now().millisecondsSinceEpoch}'
        : nameController.text.trim();
    final streamName = streamNameController.text.trim().isEmpty
        ? name
        : streamNameController.text.trim();
    final payload = <String, Object?>{
      'name': name,
      'type': taskType,
      'priority': int.tryParse(priorityController.text) ?? 50,
      'common': _clean({
        'created_by': createdByController.text,
        'callback_url': callbackController.text,
        'labels': _csv(labelsController.text),
      }),
      'input': buildTaskInputPayload(
        inputKind: inputKind,
        sourceMode: sourceMode,
        loopEnabled: loopEnabled,
        url: sourceController.text,
        group: inputGroupController.text,
        port: inputPortController.text,
        interfaceName: interfaceNameController.text,
        interfaceIp: interfaceIpController.text,
        ttl: ttlController.text,
      ),
      'process': _clean({'mode': processMode}),
      'schedule': _clean({
        'start_mode': startMode,
        'start_at': startAtController.text,
        'cron': cronController.text,
      }),
      'resource':
          _clean({'required_labels': _csv(requiredLabelsController.text)}),
      'recovery': _clean({'policy': recoveryPolicy}),
    };

    if (taskType == 'stream_ingest') {
      payload['stream'] = _clean({
        'app': streamAppController.text,
        'name': streamName,
        'vhost': vhostController.text
      });
      payload['expose'] = {
        'enable_rtsp': enableRtsp,
        'enable_rtmp': enableRtmp,
        'enable_http_ts': enableHttpTs,
        'enable_http_fmp4': enableHttpFmp4,
        'enable_hls': enableHls,
      };
      payload['record'] = _clean({
        'enabled': recordEnabled,
        'format': recordFormat,
        'duration_sec': int.tryParse(durationController.text),
        'segment_sec': int.tryParse(segmentController.text),
        'as_player': recordAsPlayer,
      });
    }
    if (taskType != 'stream_ingest' || publishKind.isNotEmpty) {
      payload['publish'] = _clean({
        'kind': publishKind,
        'url': publishUrlController.text,
        'group': publishGroupController.text,
        'port': int.tryParse(publishPortController.text),
        'interface_name': interfaceNameController.text,
        'interface_ip': interfaceIpController.text,
        'ttl': int.tryParse(ttlController.text),
        'format': publishFormat,
      });
    }
    return _clean(payload);
  }

  Map<String, Object?> _clean(Map<String, Object?> map) {
    return cleanTaskPayloadMap(map);
  }

  List<String> _csv(String value) => value
      .split(',')
      .map((item) => item.trim())
      .where((item) => item.isNotEmpty)
      .toList();

  Future<void> _preview(AppController controller) async {
    final payload =
        await controller.api('POST', '/api/v1/tasks/preview', body: _payload());
    setState(() => result = prettyJson(payload));
  }

  Future<void> _create(AppController controller) async {
    final payload =
        await controller.api('POST', '/api/v1/tasks', body: _payload());
    setState(() => result = prettyJson(payload));
  }

  Future<void> _run(
      BuildContext context, Future<void> Function() action) async {
    try {
      await action();
      if (context.mounted) {
        ScaffoldMessenger.of(context)
            .showSnackBar(const SnackBar(content: Text('操作完成')));
      }
    } catch (error) {
      if (context.mounted) {
        ScaffoldMessenger.of(context)
            .showSnackBar(SnackBar(content: Text(error.toString())));
      }
    }
  }
}

class _SectionTitle extends StatelessWidget {
  const _SectionTitle(this.text);

  final String text;

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.only(top: 16, bottom: 8),
      child: Text(text,
          style: const TextStyle(fontSize: 16, fontWeight: FontWeight.w700)),
    );
  }
}

class _Grid extends StatelessWidget {
  const _Grid(this.children);

  final List<Widget> children;

  @override
  Widget build(BuildContext context) {
    return Wrap(spacing: 12, runSpacing: 12, children: children);
  }
}

class _TextFieldBox extends StatelessWidget {
  const _TextFieldBox(this.label, this.controller, {this.width = 220});

  final String label;
  final TextEditingController controller;
  final double width;

  @override
  Widget build(BuildContext context) {
    return SizedBox(
        width: width,
        child: TextField(
            controller: controller,
            decoration: InputDecoration(labelText: label)));
  }
}

class _SelectBox extends StatelessWidget {
  const _SelectBox(this.label, this.value, this.options, this.onChanged);

  final String label;
  final String value;
  final List<String> options;
  final ValueChanged<String> onChanged;

  @override
  Widget build(BuildContext context) {
    return SizedBox(
      width: 220,
      child: DropdownButtonFormField<String>(
        initialValue: options.contains(value) ? value : options.first,
        decoration: InputDecoration(labelText: label),
        items: options
            .map((item) => DropdownMenuItem(
                value: item, child: Text(item.isEmpty ? '不设置' : item)))
            .toList(),
        onChanged: (value) {
          if (value != null) onChanged(value);
        },
      ),
    );
  }
}

class _SwitchBox extends StatelessWidget {
  const _SwitchBox(this.label, this.value, this.onChanged);

  final String label;
  final bool value;
  final ValueChanged<bool> onChanged;

  @override
  Widget build(BuildContext context) {
    return SizedBox(
      width: 180,
      child: SwitchListTile(
        contentPadding: EdgeInsets.zero,
        title: Text(label),
        value: value,
        onChanged: onChanged,
      ),
    );
  }
}
