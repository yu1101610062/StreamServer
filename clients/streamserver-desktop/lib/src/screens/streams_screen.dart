import 'dart:math' as math;

import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../core/widgets/stream_data_grid.dart';
import '../state.dart';
import '../utils.dart';
import '../widgets/data_panel.dart';
import 'screen_helpers.dart';

class StreamsScreen extends StatefulWidget {
  const StreamsScreen({this.schemaFilter, super.key});

  final String? schemaFilter;

  @override
  State<StreamsScreen> createState() => _StreamsScreenState();
}

class _StreamsScreenState extends State<StreamsScreen> {
  final appController = TextEditingController();
  final streamController = TextEditingController();
  final taskController = TextEditingController();
  final nodeController = TextEditingController();
  String schema = '';
  String hasViewer = '';
  int refreshSeed = 0;

  @override
  void initState() {
    super.initState();
    schema = widget.schemaFilter ?? '';
  }

  @override
  void didUpdateWidget(covariant StreamsScreen oldWidget) {
    super.didUpdateWidget(oldWidget);
    if (oldWidget.schemaFilter != widget.schemaFilter) {
      schema = widget.schemaFilter ?? '';
      refreshSeed++;
    }
  }

  @override
  void dispose() {
    appController.dispose();
    streamController.dispose();
    taskController.dispose();
    nodeController.dispose();
    super.dispose();
  }

  void _refresh() => setState(() => refreshSeed++);

  @override
  Widget build(BuildContext context) {
    final isMulticast = widget.schemaFilter == 'rtp';
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        PageHeader(
          title: isMulticast ? '组播中心' : '流中心',
          description:
              isMulticast ? '聚焦 RTP/组播相关在线流和任务绑定。' : '查看在线内部流、播放地址、观众数和关流入口。',
        ),
        Surface(
          child: FilterBar(
            onApply: _refresh,
            onReset: () {
              appController.clear();
              streamController.clear();
              taskController.clear();
              nodeController.clear();
              schema = widget.schemaFilter ?? '';
              hasViewer = '';
              _refresh();
            },
            children: [
              SmallSelect(
                label: '协议',
                value: schema,
                options: widget.schemaFilter == null
                    ? const ['', 'rtsp', 'rtmp', 'hls', 'http-flv', 'rtp']
                    : [widget.schemaFilter!],
                onChanged: (value) => setState(() => schema = value),
              ),
              SmallTextField(
                  controller: appController,
                  label: 'App',
                  onSubmitted: (_) => _refresh()),
              SmallTextField(
                  controller: streamController,
                  label: 'Stream',
                  onSubmitted: (_) => _refresh()),
              SmallTextField(
                  controller: taskController,
                  label: '任务 ID',
                  onSubmitted: (_) => _refresh()),
              SmallTextField(
                  controller: nodeController,
                  label: '节点 ID',
                  onSubmitted: (_) => _refresh()),
              SmallSelect(
                label: '观众',
                value: hasViewer,
                options: const ['', 'true', 'false'],
                onChanged: (value) => setState(() => hasViewer = value),
                width: 130,
              ),
            ],
          ),
        ),
        const SizedBox(height: 12),
        AsyncDataPanel(
          key: ValueKey(refreshSeed),
          loader: (controller) => controller.api(
            'GET',
            '/api/v1/streams',
            query: cleanQuery({
              'schema': schema,
              'app': appController.text,
              'stream': streamController.text,
              'task_id': taskController.text,
              'node_id': nodeController.text,
              'has_viewer': hasViewer,
            }),
          ),
          builder: (context, data) {
            final rows = rowsFrom((data as Map)['value']);
            return Surface(
              child: LayoutBuilder(
                builder: (context, constraints) {
                  if (constraints.maxWidth < 820) {
                    return _CompactStreamsList(
                      rows: rows,
                      onDone: _refresh,
                    );
                  }
                  return StreamDataGrid(
                    height: _streamGridHeight(context),
                    rowHeight: 244,
                    rows: rows,
                    columns: [
                      const StreamGridColumn(
                        title: '协议',
                        field: 'schema',
                        width: 100,
                      ),
                      StreamGridColumn(
                        title: '应用/流',
                        field: 'stream',
                        width: 230,
                        renderer: (context, row, value) => gridTextCell(
                          context,
                          '${row['app']}/${row['stream']}',
                          fontWeight: FontWeight.w800,
                          maxWidth: 220,
                        ),
                      ),
                      StreamGridColumn(
                        title: '任务',
                        field: 'task_id',
                        width: 220,
                        renderer: (context, row, value) => gridTextCell(
                          context,
                          row['task_name'] ?? row['task_id'],
                          maxWidth: 210,
                        ),
                      ),
                      StreamGridColumn(
                        title: '节点',
                        field: 'node_id',
                        width: 100,
                        renderer: (context, row, value) =>
                            Text(shortId(row['node_id'])),
                      ),
                      const StreamGridColumn(
                        title: '观众',
                        field: 'viewer_count',
                        width: 80,
                      ),
                      StreamGridColumn(
                        title: '码率',
                        field: 'bitrate_kbps',
                        width: 110,
                        renderer: (context, row, value) =>
                            Text('${row['bitrate_kbps'] ?? 0} kbps'),
                      ),
                      StreamGridColumn(
                        title: '播放地址',
                        field: 'play_urls',
                        width: 620,
                        minWidth: 420,
                        renderer: (context, row, value) {
                          final urls = (row['play_urls'] as List?)
                                  ?.map((url) => '$url')
                                  .toList() ??
                              const <String>[];
                          return PlayableUrlList(
                            urls: urls,
                            title: '${row['app']}/${row['stream']}',
                            maxWidth: 580,
                            maxVisibleItems: 2,
                          );
                        },
                      ),
                      StreamGridColumn(
                        title: '操作',
                        field: 'id',
                        width: 110,
                        minWidth: 96,
                        renderer: (context, row, value) => TextButton.icon(
                          onPressed: () =>
                              _CloseStreamButton(row: row, onDone: _refresh)
                                  .close(context),
                          icon: const Icon(LucideIcons.link2Off, size: 16),
                          label: const Text('关流'),
                        ),
                      ),
                    ],
                  );
                },
              ),
            );
          },
        ),
      ],
    );
  }
}

double _streamGridHeight(BuildContext context) {
  final height = MediaQuery.sizeOf(context).height;
  final topChrome = 34.0;
  final topCommand = height < 720 ? 72.0 : 86.0;
  const contentPadding = 46.0;
  const pageHeader = 74.0;
  const filterPanel = 150.0;
  const gapsAndMargins = 44.0;
  final available = height -
      topChrome -
      topCommand -
      contentPadding -
      pageHeader -
      filterPanel -
      gapsAndMargins;
  return math.max(420.0, available);
}

class _CompactStreamsList extends StatelessWidget {
  const _CompactStreamsList({required this.rows, required this.onDone});

  final List<Map<String, Object?>> rows;
  final VoidCallback onDone;

  @override
  Widget build(BuildContext context) {
    if (rows.isEmpty) {
      return const SizedBox(
        height: 110,
        child: Center(child: Text('暂无在线流')),
      );
    }
    return Column(
      children: [
        for (var index = 0; index < rows.length; index++) ...[
          _CompactStreamItem(row: rows[index], onDone: onDone),
          if (index != rows.length - 1)
            const Divider(height: 24, color: Color(0xffe4e8f0)),
        ],
      ],
    );
  }
}

class _CompactStreamItem extends StatelessWidget {
  const _CompactStreamItem({required this.row, required this.onDone});

  final Map<String, Object?> row;
  final VoidCallback onDone;

  @override
  Widget build(BuildContext context) {
    final urls = (row['play_urls'] as List?)?.cast<Object?>() ?? const [];
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Expanded(
              child: Text(
                '${row['app']}/${row['stream']}',
                softWrap: true,
                style: const TextStyle(fontWeight: FontWeight.w700),
              ),
            ),
            const SizedBox(width: 10),
            StatusBadge(status: row['schema']),
          ],
        ),
        const SizedBox(height: 10),
        Wrap(
          spacing: 14,
          runSpacing: 8,
          children: [
            _StreamMeta(label: '任务', value: row['task_name'] ?? row['task_id']),
            _StreamMeta(label: '节点', value: shortId(row['node_id'])),
            _StreamMeta(label: '观众', value: row['viewer_count']),
            _StreamMeta(label: '码率', value: '${row['bitrate_kbps'] ?? 0} kbps'),
          ],
        ),
        if (urls.isNotEmpty) ...[
          const SizedBox(height: 8),
          PlayableUrlList(
            urls: urls.map((url) => '$url').toList(),
            title: '${row['app']}/${row['stream']}',
            maxVisibleItems: 3,
          ),
        ],
        const SizedBox(height: 8),
        _CloseStreamButton(row: row, onDone: onDone),
      ],
    );
  }
}

class _StreamMeta extends StatelessWidget {
  const _StreamMeta({required this.label, required this.value});

  final String label;
  final Object? value;

  @override
  Widget build(BuildContext context) {
    return ConstrainedBox(
      constraints: const BoxConstraints(maxWidth: 300),
      child: RichText(
        text: TextSpan(
          style: const TextStyle(color: Color(0xff1d2433), fontSize: 13),
          children: [
            TextSpan(
              text: '$label：',
              style: const TextStyle(
                color: Color(0xff5b6477),
                fontWeight: FontWeight.w600,
              ),
            ),
            TextSpan(text: textValue(value)),
          ],
        ),
        softWrap: true,
      ),
    );
  }
}

class _CloseStreamButton extends StatelessWidget {
  const _CloseStreamButton({required this.row, required this.onDone});

  final Map<String, Object?> row;
  final VoidCallback onDone;

  @override
  Widget build(BuildContext context) {
    return TextButton.icon(
      onPressed: () => close(context),
      icon: const Icon(Icons.link_off),
      label: const Text('关流'),
    );
  }

  Future<void> close(BuildContext context) async {
    final confirmed = await confirmAction(
      context,
      title: '关闭在线流',
      message: '确认关闭 ${row['schema']} ${row['app']}/${row['stream']}？',
      confirmLabel: '关流',
      destructive: true,
    );
    if (!confirmed) return;
    if (!context.mounted) return;
    final controller = AppScope.of(context);
    try {
      await controller.api(
        'POST',
        '/api/v1/debug/zlm/close-stream',
        body: {
          'node_id': row['node_id'],
          'schema': row['schema'],
          'vhost': row['vhost'] ?? '__defaultVhost__',
          'app': row['app'],
          'stream': row['stream'],
          'force': false,
        },
      );
      if (context.mounted) showResult(context, '关流请求已提交');
      onDone();
    } catch (cause) {
      if (context.mounted) showResult(context, cause.toString());
    }
  }
}
