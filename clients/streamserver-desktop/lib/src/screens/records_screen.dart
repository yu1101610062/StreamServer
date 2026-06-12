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
              SmallTextField(
                  controller: taskController,
                  label: '任务 ID',
                  onSubmitted: (_) => _refresh(resetPage: true)),
              SmallTextField(
                  controller: streamController,
                  label: '流名',
                  onSubmitted: (_) => _refresh(resetPage: true)),
              SmallTextField(
                  controller: dateFromController,
                  label: '开始时间',
                  onSubmitted: (_) => _refresh(resetPage: true)),
              SmallTextField(
                  controller: dateToController,
                  label: '结束时间',
                  onSubmitted: (_) => _refresh(resetPage: true)),
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
                  child: PagerBar(
                      page: page,
                      pageSize: pageSize,
                      total: total,
                      onPageChanged: (value) {
                        page = value;
                        _refresh();
                      }),
                ),
                Surface(
                  child: LayoutBuilder(
                    builder: (context, constraints) {
                      if (constraints.maxWidth < 820) {
                        return _CompactRecordsList(rows);
                      }
                      return SingleChildScrollView(
                        scrollDirection: Axis.horizontal,
                        child: DataTable(
                          dataRowMinHeight: 56,
                          dataRowMaxHeight: 240,
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
                              DataCell(WrappedTextCell(
                                  value: row['task_name'] ?? row['task_id'],
                                  maxWidth: 240)),
                              DataCell(WrappedTextCell(
                                  value: _streamLabel(row), maxWidth: 260)),
                              DataCell(WrappedTextCell(
                                  value: row['file_path'],
                                  maxWidth: 360,
                                  selectable: true)),
                              DataCell(Text(bytesLabel(row['file_size']))),
                              DataCell(WrappedTextCell(
                                  value: row['created_at'], maxWidth: 220)),
                              DataCell(FullUrlText(value: row['http_url'])),
                              DataCell(_MediaActions(url: url)),
                            ]);
                          }).toList(),
                        ),
                      );
                    },
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

class _CompactRecordsList extends StatelessWidget {
  const _CompactRecordsList(this.rows);

  final List<Map<String, Object?>> rows;

  @override
  Widget build(BuildContext context) {
    if (rows.isEmpty) {
      return const SizedBox(
        height: 110,
        child: Center(child: Text('暂无录像')),
      );
    }
    return Column(
      children: [
        for (var index = 0; index < rows.length; index++) ...[
          _CompactRecordItem(rows[index]),
          if (index != rows.length - 1)
            const Divider(height: 24, color: Color(0xffe4e8f0)),
        ],
      ],
    );
  }
}

class _CompactRecordItem extends StatelessWidget {
  const _CompactRecordItem(this.row);

  final Map<String, Object?> row;

  @override
  Widget build(BuildContext context) {
    final url = '${row['http_url'] ?? ''}';
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Text(
          textValue(row['task_name'] ?? row['task_id']),
          softWrap: true,
          style: const TextStyle(fontWeight: FontWeight.w700),
        ),
        const SizedBox(height: 10),
        Wrap(
          spacing: 14,
          runSpacing: 8,
          children: [
            _RecordMeta(label: '流', value: _streamLabel(row)),
            _RecordMeta(label: '大小', value: bytesLabel(row['file_size'])),
            _RecordMeta(label: '时间', value: row['created_at']),
          ],
        ),
        const SizedBox(height: 8),
        SelectableText(textValue(row['file_path']),
            style: const TextStyle(fontSize: 12)),
        if (url.isNotEmpty) ...[
          const SizedBox(height: 8),
          FullUrlText(value: url, maxWidth: 680),
        ],
        const SizedBox(height: 8),
        _MediaActions(url: url),
      ],
    );
  }
}

class _RecordMeta extends StatelessWidget {
  const _RecordMeta({required this.label, required this.value});

  final String label;
  final Object? value;

  @override
  Widget build(BuildContext context) {
    return ConstrainedBox(
      constraints: const BoxConstraints(maxWidth: 320),
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

String _streamLabel(Map<String, Object?> row) => [
      row['vhost'],
      row['app'],
      row['stream']
    ].where((value) => value != null).join('/');

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
