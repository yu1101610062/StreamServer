import 'package:flutter/material.dart';

import '../state.dart';
import '../utils.dart';
import '../widgets/data_panel.dart';
import 'screen_helpers.dart';

class RecordsScreen extends StatefulWidget {
  const RecordsScreen({super.key});

  @override
  State<RecordsScreen> createState() => _RecordsScreenState();
}

class _RecordsScreenState extends State<RecordsScreen> {
  final taskController = TextEditingController();
  final streamController = TextEditingController();
  final dateFromController = TextEditingController();
  final dateToController = TextEditingController();
  int page = 1;
  final int pageSize = 50;
  int refreshSeed = 0;

  @override
  void dispose() {
    taskController.dispose();
    streamController.dispose();
    dateFromController.dispose();
    dateToController.dispose();
    super.dispose();
  }

  void _refresh({bool resetPage = false}) {
    setState(() {
      if (resetPage) page = 1;
      refreshSeed++;
    });
  }

  @override
  Widget build(BuildContext context) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        const PageHeader(
          title: '录像中心',
          description: '按任务、流名和时间检索录像，并通过 Rust 播放器后端打开 HTTP/HLS/MP4 地址。',
        ),
        Surface(
          child: FilterBar(
            onApply: () => _refresh(resetPage: true),
            onReset: () {
              taskController.clear();
              streamController.clear();
              dateFromController.clear();
              dateToController.clear();
              _refresh(resetPage: true);
            },
            children: [
              SmallTextField(controller: taskController, label: '任务 ID', onSubmitted: (_) => _refresh(resetPage: true)),
              SmallTextField(controller: streamController, label: '流名', onSubmitted: (_) => _refresh(resetPage: true)),
              SmallTextField(controller: dateFromController, label: '开始时间', onSubmitted: (_) => _refresh(resetPage: true)),
              SmallTextField(controller: dateToController, label: '结束时间', onSubmitted: (_) => _refresh(resetPage: true)),
            ],
          ),
        ),
        const SizedBox(height: 12),
        AsyncDataPanel(
          key: ValueKey(refreshSeed),
          loader: (controller) => controller.api(
            'GET',
            '/api/v1/records',
            query: cleanQuery({
              'task_id': taskController.text,
              'stream': streamController.text,
              'date_from': dateFromController.text,
              'date_to': dateToController.text,
              'page': page,
              'page_size': pageSize,
            }),
          ),
          builder: (context, data) {
            final payload = (data as Map).cast<String, Object?>();
            final rows = rowsFrom(payload['items']);
            final total = (payload['total'] as num?)?.toInt() ?? rows.length;
            return Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Align(
                  alignment: Alignment.centerRight,
                  child: PagerBar(page: page, pageSize: pageSize, total: total, onPageChanged: (value) {
                    page = value;
                    _refresh();
                  }),
                ),
                Surface(
                  child: SingleChildScrollView(
                    scrollDirection: Axis.horizontal,
                    child: DataTable(
                      columns: const [
                        DataColumn(label: Text('任务')),
                        DataColumn(label: Text('流')),
                        DataColumn(label: Text('路径')),
                        DataColumn(label: Text('大小')),
                        DataColumn(label: Text('时间')),
                        DataColumn(label: Text('HTTP 地址')),
                        DataColumn(label: Text('操作')),
                      ],
                      rows: rows.map((row) {
                        final url = '${row['http_url'] ?? ''}';
                        return DataRow(cells: [
                          DataCell(Text(textValue(row['task_name'] ?? row['task_id']))),
                          DataCell(Text([row['vhost'], row['app'], row['stream']].where((value) => value != null).join('/'))),
                          DataCell(Text(textValue(row['file_path']))),
                          DataCell(Text(bytesLabel(row['file_size']))),
                          DataCell(Text(textValue(row['created_at']))),
                          DataCell(SelectableText(textValue(row['http_url']))),
                          DataCell(_MediaActions(url: url)),
                        ]);
                      }).toList(),
                    ),
                  ),
                ),
              ],
            );
          },
        ),
      ],
    );
  }
}

class _MediaActions extends StatelessWidget {
  const _MediaActions({required this.url});

  final String url;

  @override
  Widget build(BuildContext context) {
    final enabled = url.startsWith('http://') || url.startsWith('https://');
    return Wrap(
      spacing: 4,
      children: [
        TextButton.icon(
          onPressed: enabled ? () => _open(context) : null,
          icon: const Icon(Icons.play_arrow),
          label: const Text('播放'),
        ),
        IconButton(
          tooltip: '复制地址',
          onPressed: enabled ? () => copyText(context, url) : null,
          icon: const Icon(Icons.copy),
        ),
      ],
    );
  }

  Future<void> _open(BuildContext context) async {
    AppScope.of(context).playMedia(url, title: '录像播放');
    showResult(context, '已打开内嵌播放器');
  }
}
