import 'package:flutter/material.dart';

import '../state.dart';
import '../utils.dart';
import '../widgets/data_panel.dart';
import 'screen_helpers.dart';

class ArtifactsScreen extends StatefulWidget {
  const ArtifactsScreen({super.key});

  @override
  State<ArtifactsScreen> createState() => _ArtifactsScreenState();
}

class _ArtifactsScreenState extends State<ArtifactsScreen> {
  final taskController = TextEditingController();
  final dateFromController = TextEditingController();
  final dateToController = TextEditingController();
  String kindFilter = '';
  int page = 1;
  final int pageSize = 50;
  int refreshSeed = 0;

  @override
  void dispose() {
    taskController.dispose();
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
          title: '文件产物',
          description: '查看桥接输出、转码输出和 VOD 快录文件，并复制或打开 HTTP 地址。',
        ),
        Surface(
          child: FilterBar(
            onApply: () => _refresh(resetPage: true),
            onReset: () {
              taskController.clear();
              dateFromController.clear();
              dateToController.clear();
              kindFilter = '';
              _refresh(resetPage: true);
            },
            children: [
              SmallSelect(
                label: '产物类型',
                value: kindFilter,
                options: const ['', 'transcode_output', 'bridge_output', 'stream_ingest_record'],
                onChanged: (value) => setState(() => kindFilter = value),
                width: 240,
              ),
              SmallTextField(controller: taskController, label: '任务 ID', onSubmitted: (_) => _refresh(resetPage: true)),
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
            '/api/v1/file-artifacts',
            query: cleanQuery({
              'artifact_kind': kindFilter,
              'task_id': taskController.text,
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
                        DataColumn(label: Text('类型')),
                        DataColumn(label: Text('任务')),
                        DataColumn(label: Text('文件名')),
                        DataColumn(label: Text('路径')),
                        DataColumn(label: Text('大小')),
                        DataColumn(label: Text('HTTP 地址')),
                        DataColumn(label: Text('操作')),
                      ],
                      rows: rows.map((row) {
                        final url = '${row['http_url'] ?? ''}';
                        return DataRow(cells: [
                          DataCell(Text(textValue(row['artifact_kind']))),
                          DataCell(Text(textValue(row['task_name'] ?? row['task_id']))),
                          DataCell(Text(textValue(row['file_name']))),
                          DataCell(Text(textValue(row['file_path']))),
                          DataCell(Text(bytesLabel(row['file_size']))),
                          DataCell(SelectableText(textValue(row['http_url']))),
                          DataCell(_ArtifactActions(url: url)),
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

class _ArtifactActions extends StatelessWidget {
  const _ArtifactActions({required this.url});

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
          label: const Text('打开'),
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
    AppScope.of(context).playMedia(url, title: '文件产物播放');
    showResult(context, '已打开内嵌播放器');
  }
}
