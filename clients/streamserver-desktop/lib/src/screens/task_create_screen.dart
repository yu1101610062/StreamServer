import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../core/theme/stream_theme.dart';
import '../state.dart';
import '../utils.dart';
import '../widgets/app_select_field.dart';
import '../widgets/data_panel.dart';
import 'screen_helpers.dart';

const _multicastInputKinds = {
  'udp_mpegts_multicast',
  'rtp_multicast',
};
const _portInputKinds = {
  'udp_mpegts_multicast',
  'rtp_multicast',
  'gb_rtp',
};
const double _taskFormControlHeight = 44;

bool taskInputUsesUrl(String kind) =>
    !_multicastInputKinds.contains(kind) && kind != 'gb_rtp';

bool taskInputUsesGroup(String kind) => _multicastInputKinds.contains(kind);

bool taskInputUsesPort(String kind) => _portInputKinds.contains(kind);

Map<String, Object?> buildTaskInputPayload({
  required String inputKind,
  required String sourceMode,
  required bool loopEnabled,
  required String startOffset,
  required String url,
  required String group,
  required String port,
  required String interfaceName,
  required String interfaceIp,
  required String ttl,
}) {
  final startOffsetSec = int.tryParse(startOffset.trim());
  return cleanTaskPayloadMap({
    'kind': inputKind,
    'source_mode': sourceMode,
    'loop_enabled': loopEnabled,
    if (sourceMode == 'vod' && !loopEnabled) 'start_offset_sec': startOffsetSec,
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
  final startOffsetController = TextEditingController();
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

  bool get _showStartOffset =>
      taskType == 'stream_ingest' && sourceMode == 'vod';

  void _clearStartOffsetIfUnsupported() {
    if (!_showStartOffset || loopEnabled) {
      startOffsetController.clear();
    }
  }

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
      startOffsetController,
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
        LayoutBuilder(
          builder: (context, constraints) {
            final wide = constraints.maxWidth >= 1040;
            final selector = _ScenarioSelector(
              value: scenario,
              onChanged: _applyScenario,
            );
            final form = scenario == 'expert'
                ? _ExpertTaskForm(
                    controller: expertController,
                    result: result,
                    onPreview: () => _run(context, () => _preview(controller)),
                    onCreate: () => _run(context, () => _create(controller)),
                  )
                : _GuidedTaskForm(
                    result: result,
                    children: [
                      _FormSection(
                        title: '基础信息',
                        description: '定义任务名称、优先级、创建者和业务侧回调信息。',
                        icon: LucideIcons.badgeInfo,
                        children: [
                          _TextFieldBox('任务名称', nameController),
                          _TextFieldBox('优先级', priorityController),
                          _TextFieldBox('创建者', createdByController),
                          _TextFieldBox('回调 URL', callbackController),
                          _TextFieldBox('业务标签，逗号分隔', labelsController),
                        ],
                      ),
                      _FormSection(
                        title: '输入与处理',
                        description: '选择输入来源、源类型和处理策略；组播和 GB RTP 会显示地址/端口字段。',
                        icon: LucideIcons.radioReceiver,
                        children: [
                          _SelectBox(
                            '任务类型',
                            taskType,
                            const [
                              'stream_ingest',
                              'stream_bridge',
                              'file_transcode'
                            ],
                            (value) => setState(() => taskType = value),
                          ),
                          _SelectBox(
                            '输入类型',
                            inputKind,
                            const [
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
                            (value) => setState(() => inputKind = value),
                          ),
                          _SelectBox(
                            '源模式',
                            sourceMode,
                            const ['live', 'vod'],
                            (value) => setState(() {
                              sourceMode = value;
                              _clearStartOffsetIfUnsupported();
                            }),
                          ),
                          _SelectBox(
                            '处理模式',
                            processMode,
                            const ['copy_or_transcode', 'copy', 'transcode'],
                            (value) => setState(() => processMode = value),
                          ),
                          if (taskInputUsesUrl(inputKind))
                            _TextFieldBox('输入 URL / 文件路径', sourceController),
                          if (taskInputUsesGroup(inputKind))
                            _TextFieldBox('输入组播地址', inputGroupController),
                          if (taskInputUsesPort(inputKind))
                            _TextFieldBox(
                              inputKind == 'gb_rtp' ? 'GB RTP 监听端口' : '输入端口',
                              inputPortController,
                            ),
                          if (_showStartOffset)
                            _TextFieldBox(
                              '开始偏移秒',
                              startOffsetController,
                              enabled: !loopEnabled,
                            ),
                          _SwitchBox(
                            '循环 VOD',
                            loopEnabled,
                            (value) => setState(() {
                              loopEnabled = value;
                              _clearStartOffsetIfUnsupported();
                            }),
                          ),
                        ],
                      ),
                      _FormSection(
                        title: '内部流与播放协议',
                        description: '配置 StreamServer 内部流命名和对外播放协议。',
                        icon: LucideIcons.playSquare,
                        children: [
                          _TextFieldBox('App', streamAppController),
                          _TextFieldBox('Stream', streamNameController),
                          _TextFieldBox('Vhost', vhostController),
                          _SwitchBox(
                            'RTSP',
                            enableRtsp,
                            (value) => setState(() => enableRtsp = value),
                          ),
                          _SwitchBox(
                            'RTMP',
                            enableRtmp,
                            (value) => setState(() => enableRtmp = value),
                          ),
                          _SwitchBox(
                            'HTTP-TS',
                            enableHttpTs,
                            (value) => setState(() => enableHttpTs = value),
                          ),
                          _SwitchBox(
                            'HTTP-FMP4',
                            enableHttpFmp4,
                            (value) => setState(() => enableHttpFmp4 = value),
                          ),
                          _SwitchBox(
                            'HLS',
                            enableHls,
                            (value) => setState(() => enableHls = value),
                          ),
                        ],
                      ),
                      _FormSection(
                        title: '发布与网络',
                        description: '发布到文件、组播或 RTMP 推流时填写，未设置则仅创建内部流。',
                        icon: LucideIcons.network,
                        children: [
                          _SelectBox(
                            '发布类型',
                            publishKind,
                            const [
                              '',
                              'file',
                              'udp_mpegts_multicast',
                              'rtp_multicast',
                              'rtmp_push'
                            ],
                            (value) => setState(() => publishKind = value),
                          ),
                          _SelectBox(
                            '文件格式',
                            publishFormat,
                            const ['', 'mp4', 'hls', 'mpegts', 'flv'],
                            (value) => setState(() => publishFormat = value),
                          ),
                          _TextFieldBox('发布 URL / 文件路径', publishUrlController),
                          _TextFieldBox('组播地址', publishGroupController),
                          _TextFieldBox('端口', publishPortController),
                          _TextFieldBox('网卡名', interfaceNameController),
                          _TextFieldBox('绑定 IP', interfaceIpController),
                          _TextFieldBox('TTL', ttlController),
                        ],
                      ),
                      _FormSection(
                        title: '录制、恢复与调度',
                        description: '设置录制参数、恢复策略、启动模式和节点资源标签。',
                        icon: LucideIcons.calendarClock,
                        children: [
                          _SwitchBox(
                            '启用录制',
                            recordEnabled,
                            (value) => setState(() => recordEnabled = value),
                          ),
                          _SelectBox(
                            '录制格式',
                            recordFormat,
                            const ['mp4', 'hls', 'both'],
                            (value) => setState(() => recordFormat = value),
                          ),
                          _TextFieldBox('录制时长秒', durationController),
                          _TextFieldBox('分段秒', segmentController),
                          _SwitchBox(
                            '按播放器模式录制',
                            recordAsPlayer,
                            (value) => setState(() => recordAsPlayer = value),
                          ),
                          _SelectBox(
                            '恢复策略',
                            recoveryPolicy,
                            const ['auto', 'none'],
                            (value) => setState(() => recoveryPolicy = value),
                          ),
                          _SelectBox(
                            '启动模式',
                            startMode,
                            const ['immediate', 'manual', 'at', 'cron'],
                            (value) => setState(() => startMode = value),
                          ),
                          _TextFieldBox('启动时间 RFC3339', startAtController),
                          _TextFieldBox('Cron', cronController),
                          _TextFieldBox(
                              '节点标签要求，逗号分隔', requiredLabelsController),
                        ],
                      ),
                      _ActionSection(
                        result: result,
                        onPreview: () =>
                            _run(context, () => _preview(controller)),
                        onCreate: () =>
                            _run(context, () => _create(controller)),
                      ),
                    ],
                  );
            if (!wide) {
              return Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  selector,
                  const SizedBox(height: 14),
                  form,
                ],
              );
            }
            return Row(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                SizedBox(width: 260, child: selector),
                const SizedBox(width: 16),
                Expanded(child: form),
              ],
            );
          },
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
      _clearStartOffsetIfUnsupported();
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
        startOffset:
            _showStartOffset && !loopEnabled ? startOffsetController.text : '',
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
        showResult(context, '操作完成', tone: InlineStatusTone.success);
      }
    } catch (error) {
      if (context.mounted) {
        showResult(
          context,
          error.toString(),
          tone: InlineStatusTone.danger,
        );
      }
    }
  }
}

class _ScenarioSelector extends StatelessWidget {
  const _ScenarioSelector({required this.value, required this.onChanged});

  final String value;
  final ValueChanged<String> onChanged;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    final items = const [
      _ScenarioSpec(
        value: 'live-ingest',
        title: '实时流接入',
        description: 'RTSP/RTMP/HLS 等在线流进入 StreamServer。',
        icon: LucideIcons.radio,
      ),
      _ScenarioSpec(
        value: 'ingest-record',
        title: '接入并录制',
        description: '创建在线流并同步启用录制参数。',
        icon: LucideIcons.video,
      ),
      _ScenarioSpec(
        value: 'bridge-out',
        title: '桥接输出',
        description: '接入后发布到文件、组播或 RTMP。',
        icon: LucideIcons.workflow,
      ),
      _ScenarioSpec(
        value: 'file-transcode',
        title: '离线转码',
        description: '本地/远端文件转码并输出产物。',
        icon: LucideIcons.fileVideo,
      ),
      _ScenarioSpec(
        value: 'expert',
        title: '专家 JSON',
        description: '直接提交完整任务 JSON。',
        icon: LucideIcons.braces,
      ),
    ];
    return Surface(
      padding: const EdgeInsets.all(14),
      child: LayoutBuilder(
        builder: (context, constraints) {
          final horizontal = constraints.maxWidth > 520;
          final children = [
            for (final item in items)
              _ScenarioOption(
                item: item,
                selected: value == item.value,
                onTap: () => onChanged(item.value),
              ),
          ];
          return Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text(
                '任务场景',
                style: TextStyle(
                  color: colors.textPrimary,
                  fontWeight: FontWeight.w900,
                  fontSize: 15,
                ),
              ),
              const SizedBox(height: 4),
              Text(
                '先选任务意图，再补齐必要参数。',
                style: TextStyle(color: colors.textSecondary, fontSize: 12),
              ),
              const SizedBox(height: 12),
              if (horizontal)
                Wrap(spacing: 10, runSpacing: 10, children: children)
              else
                Column(children: children),
            ],
          );
        },
      ),
    );
  }
}

class _ScenarioSpec {
  const _ScenarioSpec({
    required this.value,
    required this.title,
    required this.description,
    required this.icon,
  });

  final String value;
  final String title;
  final String description;
  final IconData icon;
}

class _ScenarioOption extends StatelessWidget {
  const _ScenarioOption({
    required this.item,
    required this.selected,
    required this.onTap,
  });

  final _ScenarioSpec item;
  final bool selected;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return InkWell(
      borderRadius: BorderRadius.circular(10),
      onTap: onTap,
      child: Container(
        width: 232,
        height: 84,
        margin: const EdgeInsets.only(bottom: 10),
        padding: const EdgeInsets.all(12),
        decoration: BoxDecoration(
          color: selected
              ? colors.primary.withValues(alpha: 0.14)
              : colors.surfaceAlt.withValues(alpha: 0.72),
          border: Border.all(
            color: selected ? colors.primary : colors.border,
            width: selected ? 1.2 : 1,
          ),
          borderRadius: BorderRadius.circular(10),
        ),
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Container(
              width: 34,
              height: 34,
              decoration: BoxDecoration(
                color: selected
                    ? colors.primary.withValues(alpha: 0.22)
                    : colors.surface,
                borderRadius: BorderRadius.circular(8),
              ),
              child: Icon(
                selected ? LucideIcons.check : item.icon,
                size: 17,
                color: selected ? colors.primary : colors.textSecondary,
              ),
            ),
            const SizedBox(width: 10),
            Expanded(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text(
                    item.title,
                    style: TextStyle(
                      color: colors.textPrimary,
                      fontWeight: FontWeight.w800,
                    ),
                  ),
                  const SizedBox(height: 4),
                  Text(
                    item.description,
                    maxLines: 2,
                    overflow: TextOverflow.ellipsis,
                    style: TextStyle(
                      color: colors.textSecondary,
                      fontSize: 12,
                      height: 1.35,
                    ),
                  ),
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }
}

class _GuidedTaskForm extends StatelessWidget {
  const _GuidedTaskForm({required this.children, required this.result});

  final List<Widget> children;
  final String? result;

  @override
  Widget build(BuildContext context) {
    return Column(
      children: [
        for (var index = 0; index < children.length; index++) ...[
          children[index],
          if (index != children.length - 1) const SizedBox(height: 14),
        ],
      ],
    );
  }
}

class _ExpertTaskForm extends StatelessWidget {
  const _ExpertTaskForm({
    required this.controller,
    required this.onPreview,
    required this.onCreate,
    this.result,
  });

  final TextEditingController controller;
  final VoidCallback onPreview;
  final VoidCallback onCreate;
  final String? result;

  @override
  Widget build(BuildContext context) {
    return Column(
      children: [
        _FormSection(
          title: '专家 JSON',
          description: '适合从 Web 管理台或 API 文档复制完整任务规格后直接调整提交。',
          icon: LucideIcons.braces,
          forceSingleColumn: true,
          children: [
            TextField(
              controller: controller,
              minLines: 18,
              maxLines: 28,
              style: const TextStyle(fontFamily: 'Menlo', fontSize: 12),
              decoration: const InputDecoration(labelText: '任务 JSON'),
            ),
          ],
        ),
        const SizedBox(height: 14),
        _ActionSection(
          result: result,
          onPreview: onPreview,
          onCreate: onCreate,
        ),
      ],
    );
  }
}

class _FormSection extends StatelessWidget {
  const _FormSection({
    required this.title,
    required this.description,
    required this.icon,
    required this.children,
    this.forceSingleColumn = false,
  });

  final String title;
  final String description;
  final IconData icon;
  final List<Widget> children;
  final bool forceSingleColumn;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return Surface(
      padding: const EdgeInsets.all(18),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Container(
                width: 34,
                height: 34,
                decoration: BoxDecoration(
                  color: colors.primary.withValues(alpha: 0.12),
                  borderRadius: BorderRadius.circular(8),
                ),
                child: Icon(icon, color: colors.primary, size: 17),
              ),
              const SizedBox(width: 12),
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Text(
                      title,
                      style: TextStyle(
                        color: colors.textPrimary,
                        fontSize: 16,
                        fontWeight: FontWeight.w900,
                      ),
                    ),
                    const SizedBox(height: 4),
                    Text(
                      description,
                      style: TextStyle(
                        color: colors.textSecondary,
                        fontSize: 12,
                        height: 1.35,
                      ),
                    ),
                  ],
                ),
              ),
            ],
          ),
          const SizedBox(height: 16),
          _FormGrid(
            forceSingleColumn: forceSingleColumn,
            children: children,
          ),
        ],
      ),
    );
  }
}

class _FormGrid extends StatelessWidget {
  const _FormGrid({
    required this.children,
    this.forceSingleColumn = false,
  });

  final List<Widget> children;
  final bool forceSingleColumn;

  @override
  Widget build(BuildContext context) {
    return LayoutBuilder(
      builder: (context, constraints) {
        final columns = forceSingleColumn
            ? 1
            : constraints.maxWidth >= 900
                ? 3
                : constraints.maxWidth >= 620
                    ? 2
                    : 1;
        final width = (constraints.maxWidth - 12 * (columns - 1)) / columns;
        return Wrap(
          spacing: 12,
          runSpacing: 12,
          children: [
            for (final child in children) SizedBox(width: width, child: child),
          ],
        );
      },
    );
  }
}

class _TextFieldBox extends StatefulWidget {
  const _TextFieldBox(this.label, this.controller, {this.enabled = true});

  final String label;
  final TextEditingController controller;
  final bool enabled;

  @override
  State<_TextFieldBox> createState() => _TextFieldBoxState();
}

class _TextFieldBoxState extends State<_TextFieldBox> {
  late final FocusNode _focusNode;

  @override
  void initState() {
    super.initState();
    _focusNode = FocusNode()..addListener(_handleFocusChanged);
  }

  @override
  void dispose() {
    _focusNode
      ..removeListener(_handleFocusChanged)
      ..dispose();
    super.dispose();
  }

  void _handleFocusChanged() => setState(() {});

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    final fillColor = Theme.of(context).inputDecorationTheme.fillColor!;
    final focused = _focusNode.hasFocus;
    return SizedBox(
      height: _taskFormControlHeight,
      child: Stack(
        clipBehavior: Clip.none,
        children: [
          Positioned.fill(
            child: Material(
              color: fillColor,
              shape: RoundedRectangleBorder(
                side: BorderSide(
                  color: focused ? colors.primary : colors.border,
                  width: focused ? 1.3 : 1,
                ),
                borderRadius: BorderRadius.circular(8),
              ),
              clipBehavior: Clip.antiAlias,
              child: TextField(
                controller: widget.controller,
                focusNode: _focusNode,
                enabled: widget.enabled,
                style: TextStyle(
                  color: widget.enabled
                      ? colors.textPrimary
                      : colors.textSecondary,
                  fontSize: 13,
                  fontWeight: FontWeight.w700,
                ),
                decoration: const InputDecoration(
                  border: InputBorder.none,
                  enabledBorder: InputBorder.none,
                  focusedBorder: InputBorder.none,
                  disabledBorder: InputBorder.none,
                  filled: false,
                  isDense: true,
                  contentPadding: EdgeInsets.symmetric(
                    horizontal: 12,
                    vertical: 13,
                  ),
                ),
              ),
            ),
          ),
          Positioned(
            left: 10,
            top: -7,
            child: DecoratedBox(
              decoration: BoxDecoration(color: colors.surface),
              child: Padding(
                padding: const EdgeInsets.symmetric(horizontal: 4),
                child: Text(
                  widget.label,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: TextStyle(
                    color: focused ? colors.primary : colors.textSecondary,
                    fontSize: 12,
                    height: 1,
                  ),
                ),
              ),
            ),
          ),
        ],
      ),
    );
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
    return AppSelectField<String>(
      label: label,
      width: double.infinity,
      height: _taskFormControlHeight,
      value: options.contains(value) ? value : options.first,
      options: [
        for (final item in options)
          AppSelectOption(
            value: item,
            label: item.isEmpty ? '不设置' : item,
          ),
      ],
      onChanged: onChanged,
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
    final colors = context.streamColors;
    return SizedBox(
      height: _taskFormControlHeight,
      child: Material(
        color:
            colors.surfaceAlt.withValues(alpha: context.isDarkMode ? 0.6 : 0.9),
        shape: RoundedRectangleBorder(
          side: BorderSide(color: colors.border),
          borderRadius: BorderRadius.circular(8),
        ),
        clipBehavior: Clip.antiAlias,
        child: InkWell(
          onTap: () => onChanged(!value),
          child: Padding(
            padding: const EdgeInsets.only(left: 12, right: 8),
            child: Row(
              children: [
                Expanded(
                  child: Text(
                    label,
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                    style: TextStyle(
                      color: colors.textPrimary,
                      fontSize: 13,
                      fontWeight: FontWeight.w800,
                    ),
                  ),
                ),
                SizedBox(
                  width: 46,
                  height: 30,
                  child: FittedBox(
                    fit: BoxFit.contain,
                    alignment: Alignment.centerRight,
                    child: Switch(
                      value: value,
                      onChanged: onChanged,
                      materialTapTargetSize: MaterialTapTargetSize.shrinkWrap,
                    ),
                  ),
                ),
              ],
            ),
          ),
        ),
      ),
    );
  }
}

class _ActionSection extends StatelessWidget {
  const _ActionSection({
    required this.onPreview,
    required this.onCreate,
    this.result,
  });

  final VoidCallback onPreview;
  final VoidCallback onCreate;
  final String? result;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return Surface(
      padding: const EdgeInsets.all(18),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          LayoutBuilder(
            builder: (context, constraints) {
              final compact = constraints.maxWidth < 560;
              final actions = [
                OutlinedButton.icon(
                  onPressed: onPreview,
                  icon: const Icon(LucideIcons.eye, size: 17),
                  label: const Text('规格预览'),
                ),
                FilledButton.icon(
                  onPressed: onCreate,
                  icon: const Icon(LucideIcons.listPlus, size: 17),
                  label: const Text('创建任务'),
                ),
              ];
              return compact
                  ? Column(
                      crossAxisAlignment: CrossAxisAlignment.stretch,
                      children: actions,
                    )
                  : Row(
                      mainAxisAlignment: MainAxisAlignment.end,
                      children: [
                        actions.first,
                        const SizedBox(width: 10),
                        actions.last,
                      ],
                    );
            },
          ),
          if (result != null) ...[
            const SizedBox(height: 14),
            DecoratedBox(
              decoration: BoxDecoration(
                color: colors.sidebar,
                border: Border.all(color: colors.border),
                borderRadius: BorderRadius.circular(8),
              ),
              child: Padding(
                padding: const EdgeInsets.all(12),
                child: SelectableText(
                  result!,
                  style: const TextStyle(
                    color: Color(0xffe5e7eb),
                    fontSize: 12,
                    height: 1.35,
                    fontFamily: 'Menlo',
                  ),
                ),
              ),
            ),
          ],
        ],
      ),
    );
  }
}
