import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../core/widgets/stream_data_grid.dart';
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
                options: const [
                  '',
                  'transcode_output',
                  'bridge_output',
                  'stream_ingest_record'
                ],
                onChanged: (value) => setState(() => kindFilter = value),
                width: 240,
              ),
              SmallTextField(
                  controller: taskController,
                  label: '任务 ID',
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
                        return _CompactArtifactsList(rows);
                      }
                      return StreamDataGrid(
                        height: 600,
                        rowHeight: 96,
                        rows: rows,
                        columns: [
                          const StreamGridColumn(
                            title: '类型',
                            field: 'artifact_kind',
                            width: 170,
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
                            title: '文件名',
                            field: 'file_name',
                            width: 230,
                            renderer: (context, row, value) => gridTextCell(
                              context,
                              value,
                              fontWeight: FontWeight.w800,
                              maxWidth: 220,
                            ),
                          ),
                          StreamGridColumn(
                            title: '路径',
                            field: 'file_path',
                            width: 320,
                            renderer: (context, row, value) =>
                                gridTextCell(context, value, maxWidth: 310),
                          ),
                          StreamGridColumn(
                            title: '大小',
                            field: 'file_size',
                            width: 110,
                            renderer: (context, row, value) =>
                                Text(bytesLabel(row['file_size'])),
                          ),
                          StreamGridColumn(
                            title: 'HTTP 地址',
                            field: 'http_url',
                            width: 460,
                            renderer: (context, row, value) =>
                                FullUrlText(value: value, maxWidth: 440),
                          ),
                          StreamGridColumn(
                            title: '操作',
                            field: 'http_url',
                            width: 110,
                            renderer: (context, row, value) =>
                                _ArtifactIconActions(
                                    url: '${row['http_url'] ?? ''}'),
                          ),
                        ],
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

class _ArtifactIconActions extends StatelessWidget {
  const _ArtifactIconActions({required this.url});

  final String url;

  @override
  Widget build(BuildContext context) {
    final enabled = url.startsWith('http://') || url.startsWith('https://');
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        IconButton(
          tooltip: '打开',
          onPressed: enabled ? () => _open(context) : null,
          icon: const Icon(LucideIcons.circlePlay, size: 17),
        ),
        IconButton(
          tooltip: '复制地址',
          onPressed: enabled ? () => copyText(context, url) : null,
          icon: const Icon(LucideIcons.copy, size: 17),
        ),
      ],
    );
  }

  Future<void> _open(BuildContext context) async {
    AppScope.of(context).playMedia(url, title: '文件产物播放');
    showResult(context, '已打开内嵌播放器');
  }
}

class _CompactArtifactsList extends StatelessWidget {
  const _CompactArtifactsList(this.rows);

  final List<Map<String, Object?>> rows;

  @override
  Widget build(BuildContext context) {
    if (rows.isEmpty) {
      return const SizedBox(
        height: 110,
        child: Center(child: Text('暂无文件产物')),
      );
    }
    return Column(
      children: [
        for (var index = 0; index < rows.length; index++) ...[
          _CompactArtifactItem(rows[index]),
          if (index != rows.length - 1)
            const Divider(height: 24, color: Color(0xffe4e8f0)),
        ],
      ],
    );
  }
}

class _CompactArtifactItem extends StatelessWidget {
  const _CompactArtifactItem(this.row);

  final Map<String, Object?> row;

  @override
  Widget build(BuildContext context) {
    final url = '${row['http_url'] ?? ''}';
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Expanded(
              child: Text(
                textValue(row['file_name']),
                softWrap: true,
                style: const TextStyle(fontWeight: FontWeight.w700),
              ),
            ),
            const SizedBox(width: 10),
            StatusBadge(status: row['artifact_kind']),
          ],
        ),
        const SizedBox(height: 10),
        Wrap(
          spacing: 14,
          runSpacing: 8,
          children: [
            _ArtifactMeta(
                label: '任务', value: row['task_name'] ?? row['task_id']),
            _ArtifactMeta(label: '大小', value: bytesLabel(row['file_size'])),
          ],
        ),
        const SizedBox(height: 8),
        SelectableText(textValue(row['file_path']),
            style: const TextStyle(fontSize: 12)),
        if (url.isNotEmpty) ...[
          const SizedBox(height: 8),
          SelectableText(url, style: const TextStyle(fontSize: 12)),
        ],
        const SizedBox(height: 8),
        _ArtifactActions(url: url),
      ],
    );
  }
}

class _ArtifactMeta extends StatelessWidget {
  const _ArtifactMeta({required this.label, required this.value});

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
