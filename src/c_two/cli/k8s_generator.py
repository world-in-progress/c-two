# src/c_two/cli/k8s_generator.py
import yaml

class K8sManifestGenerator:
    def __init__(self, project_path: str):
        self.project_path = project_path
        
    def generate_manifests(self, name: str, image: str, port: int, 
                         replicas: int, namespace: str) -> dict[str, str]:
        """Generate complete Kubernetes deployment manifests"""
        manifests = {}
        
        # Deployment
        manifests['deployment.yaml'] = self._generate_deployment(
            name, image, port, replicas, namespace
        )
        
        # Service
        manifests['service.yaml'] = self._generate_service(
            name, port, namespace
        )
        
        # ConfigMap (optional)
        manifests['configmap.yaml'] = self._generate_configmap(
            name, namespace
        )
        
        # HPA (Horizontal Pod Autoscaler)
        manifests['hpa.yaml'] = self._generate_hpa(
            name, namespace
        )
        
        # ServiceMonitor (for Prometheus)
        manifests['servicemonitor.yaml'] = self._generate_service_monitor(
            name, namespace
        )
        
        return manifests
    
    def _generate_deployment(self, name: str, image: str, port: int, 
                           replicas: int, namespace: str) -> str:
        """生成Deployment清单"""
        deployment = {
            'apiVersion': 'apps/v1',
            'kind': 'Deployment',
            'metadata': {
                'name': name,
                'namespace': namespace,
                'labels': {
                    'app': name,
                    'component': 'crm-server',
                    'managed-by': 'c-two-seeding'
                }
            },
            'spec': {
                'replicas': replicas,
                'selector': {
                    'matchLabels': {
                        'app': name
                    }
                },
                'template': {
                    'metadata': {
                        'labels': {
                            'app': name,
                            'component': 'crm-server'
                        }
                    },
                    'spec': {
                        'containers': [{
                            'name': 'crm-server',
                            'image': image,
                            'ports': [
                                {
                                    'containerPort': port,
                                    'name': 'crm-port'
                                },
                                {
                                    'containerPort': 8001,
                                    'name': 'health-port'
                                }
                            ],
                            'env': [
                                {
                                    'name': 'CRM_BIND_ADDRESS',
                                    'value': f'http://0.0.0.0:{port}'
                                },
                                {
                                    'name': 'CRM_NAME',
                                    'value': name
                                }
                            ],
                            'resources': {
                                'requests': {
                                    'memory': '256Mi',
                                    'cpu': '100m'
                                },
                                'limits': {
                                    'memory': '1Gi',
                                    'cpu': '500m'
                                }
                            },
                            'livenessProbe': {
                                'httpGet': {
                                    'path': '/health',
                                    'port': 8001
                                },
                                'initialDelaySeconds': 30,
                                'periodSeconds': 10
                            },
                            'readinessProbe': {
                                'httpGet': {
                                    'path': '/health',
                                    'port': 8001
                                },
                                'initialDelaySeconds': 5,
                                'periodSeconds': 5
                            }
                        }],
                        'restartPolicy': 'Always'
                    }
                }
            }
        }
        
        return yaml.dump(deployment, default_flow_style=False)
    
    def _generate_service(self, name: str, port: int, namespace: str) -> str:
        """生成Service清单"""
        service = {
            'apiVersion': 'v1',
            'kind': 'Service',
            'metadata': {
                'name': name,
                'namespace': namespace,
                'labels': {
                    'app': name,
                    'component': 'crm-server'
                }
            },
            'spec': {
                'selector': {
                    'app': name
                },
                'ports': [
                    {
                        'name': 'crm-port',
                        'port': port,
                        'targetPort': port,
                        'protocol': 'TCP'
                    },
                    {
                        'name': 'health-port', 
                        'port': 8001,
                        'targetPort': 8001,
                        'protocol': 'TCP'
                    }
                ],
                'type': 'ClusterIP'
            }
        }
        
        return yaml.dump(service, default_flow_style=False)
    
    def _generate_hpa(self, name: str, namespace: str) -> str:
        """Generate HPA manifest"""
        hpa = {
            'apiVersion': 'autoscaling/v2',
            'kind': 'HorizontalPodAutoscaler',
            'metadata': {
                'name': f'{name}-hpa',
                'namespace': namespace
            },
            'spec': {
                'scaleTargetRef': {
                    'apiVersion': 'apps/v1',
                    'kind': 'Deployment',
                    'name': name
                },
                'minReplicas': 1,
                'maxReplicas': 10,
                'metrics': [
                    {
                        'type': 'Resource',
                        'resource': {
                            'name': 'cpu',
                            'target': {
                                'type': 'Utilization',
                                'averageUtilization': 70
                            }
                        }
                    },
                    {
                        'type': 'Resource',
                        'resource': {
                            'name': 'memory',
                            'target': {
                                'type': 'Utilization',
                                'averageUtilization': 80
                            }
                        }
                    }
                ]
            }
        }
        
        return yaml.dump(hpa, default_flow_style=False)