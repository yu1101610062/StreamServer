import 'package:flutter/material.dart';

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
          description: isMulticast ? '聚焦 RTP/组播相关在线流和任务绑定。' : '查看在线内部流、播放地址、观众数和关流入口。',
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
                options: widget.schemaFilter == null ? const ['', 'rtsp', 'rtmp', 'hls', 'http-flv', 'rtp'] : [widget.schemaFilter!],
                onChanged: (value) => setState(() => schema = value),
              ),
              SmallTextField(controller: appController, label: 'App', onSubmitted: (_) => _refresh()),
              SmallTextField(controller: streamController, label: 'Stream', onSubmitted: (_) => _refresh()),
              SmallTextField(controller: taskController, label: '任务 ID', onSubmitted: (_) => _refresh()),
              SmallTextField(controller: nodeController, label: '节点 ID', onSubmitted: (_) => _refresh()),
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
              child: SingleChildScrollView(
                scrollDirection: Axis.horizontal,
                child: DataTable(
                  columns: const [
                    DataColumn(label: Text('协议')),
                    DataColumn(label: Text('应用/流')),
                    DataColumn(label: Text('任务')),
                    DataColumn(label: Text('节点')),
                    DataColumn(label: Text('观众')),
                    DataColumn(label: Text('码率')),
                    DataColumn(label: Text('播放地址')),
                    DataColumn(label: Text('操作')),
                  ],
                  rows: rows.map((row) {
                    final urls = (row['play_urls'] as List?)?.cast<Object?>() ?? const [];
                    return DataRow(
                      cells: [
                        DataCell(Text(textValue(row['schema']))),
                        DataCell(Text('${row['app']}/${row['stream']}')),
                        DataCell(Text(textValue(row['task_name'] ?? row['task_id']))),
                        DataCell(Text(shortId(row['node_id']))),
                        DataCell(Text(textValue(row['viewer_count']))),
                        DataCell(Text('${row['bitrate_kbps'] ?? 0} kbps')),
                        DataCell(Wrap(spacing: 4, children: urls.map((url) => _PlayButton(url: '$url')).toList())),
                        DataCell(_CloseStreamButton(row: row, onDone: _refresh)),
                      ],
                    );
                  }).toList(),
                ),
              ),
            );
          },
        ),
      ],
    );
  }
}

class _PlayButton extends StatelessWidget {
  const _PlayButton({required this.url});

  final String url;

  @override
  Widget build(BuildContext context) {
    return Wrap(
      spacing: 2,
      crossAxisAlignment: WrapCrossAlignment.center,
      children: [
        TextButton.icon(
          onPressed: () => _open(context),
          icon: const Icon(Icons.play_arrow),
          label: Text(url, overflow: TextOverflow.ellipsis),
        ),
        IconButton(
          tooltip: '复制地址',
          onPressed: () => copyText(context, url),
          icon: const Icon(Icons.copy),
        ),
      ],
    );
  }

  Future<void> _open(BuildContext context) async {
    AppScope.of(context).playMedia(url, title: url);
    showResult(context, '已打开内嵌播放器');
  }
}

class _CloseStreamButton extends StatelessWidget {
  const _CloseStreamButton({required this.row, required this.onDone});

  final Map<String, Object?> row;
  final VoidCallback onDone;

  @override
  Widget build(BuildContext context) {
    return TextButton.icon(
      onPressed: () => _close(context),
      icon: const Icon(Icons.link_off),
      label: const Text('关流'),
    );
  }

  Future<void> _close(BuildContext context) async {
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
